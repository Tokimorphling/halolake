#!/usr/bin/env node

import http from 'node:http';
import https from 'node:https';
import { performance } from 'node:perf_hooks';

const parseArgs = (argv) => {
  const values = new Map();
  for (let index = 0; index < argv.length; index += 1) {
    const item = argv[index];
    if (!item.startsWith('--')) continue;
    const key = item.slice(2);
    const next = argv[index + 1];
    if (next === undefined || next.startsWith('--')) {
      values.set(key, 'true');
    } else {
      values.set(key, next);
      index += 1;
    }
  }
  return values;
};

const args = parseArgs(process.argv.slice(2));
const url = new URL(args.get('url') ?? 'http://127.0.0.1:8080/v1/chat/completions');
const concurrency = Math.max(1, Number.parseInt(args.get('concurrency') ?? '64', 10));
const requestLimit = Math.max(1, Number.parseInt(args.get('requests') ?? '2000', 10));
const durationSeconds = Math.max(0, Number.parseFloat(args.get('duration') ?? '0'));
const warmup = Math.max(0, Number.parseInt(args.get('warmup') ?? '100', 10));
const timeoutMs = Math.max(1, Number.parseInt(args.get('timeout-ms') ?? '120000', 10));
const stream = (args.get('stream') ?? 'true') !== 'false';
const model = args.get('model') ?? 'gpt-4o';
const token = args.get('token') ?? process.env.BENCHMARK_TOKEN ?? 'benchmark-token';
const jsonOnly = args.get('json') === 'true';
const responseTailLimit = 64 * 1024;

const payload = Buffer.from(
  JSON.stringify({
    model,
    stream,
    stream_options: stream ? { include_usage: true } : undefined,
    messages: [{ role: 'user', content: 'benchmark' }],
  }),
);

const transport = url.protocol === 'https:' ? https : http;
const agent = new transport.Agent({
  keepAlive: true,
  maxSockets: concurrency,
  maxFreeSockets: concurrency,
  scheduling: 'lifo',
});

const percentile = (values, quantile) => {
  if (values.length === 0) return null;
  const sorted = [...values].sort((left, right) => left - right);
  const index = Math.min(sorted.length - 1, Math.ceil(quantile * sorted.length) - 1);
  return Number(sorted[Math.max(0, index)].toFixed(3));
};

const completionTokensFromBody = (body) => {
  const matches = [...body.matchAll(/"completion_tokens"\s*:\s*(\d+)/g)];
  if (matches.length > 0) return Number.parseInt(matches.at(-1)[1], 10);
  const outputMatches = [...body.matchAll(/"output_tokens"\s*:\s*(\d+)/g)];
  return outputMatches.length > 0 ? Number.parseInt(outputMatches.at(-1)[1], 10) : null;
};

const hasContentDelta = (payload) => {
  if (
    Array.isArray(payload?.choices) &&
    payload.choices.some((choice) =>
      [choice?.delta?.content, choice?.text].some(
        (value) => typeof value === 'string' && value.length > 0,
      ),
    )
  ) {
    return true;
  }
  if (
    payload?.type === 'content_block_delta' &&
    typeof payload?.delta?.text === 'string' &&
    payload.delta.text.length > 0
  ) {
    return true;
  }
  if (
    payload?.type === 'response.output_text.delta' &&
    typeof payload?.delta === 'string' &&
    payload.delta.length > 0
  ) {
    return true;
  }
  return (
    Array.isArray(payload?.candidates) &&
    payload.candidates.some((candidate) =>
      candidate?.content?.parts?.some(
        (part) => typeof part?.text === 'string' && part.text.length > 0,
      ),
    )
  );
};

const completionTokensFromPayload = (payload) => {
  const candidates = [
    payload?.usage?.completion_tokens,
    payload?.usage?.output_tokens,
    payload?.usageMetadata?.candidatesTokenCount,
    payload?.response?.usage?.completion_tokens,
    payload?.response?.usage?.output_tokens,
    payload?.message?.usage?.output_tokens,
  ];
  const value = candidates.find((candidate) => Number.isFinite(candidate) && candidate >= 0);
  return value === undefined ? null : Number(value);
};

const consumeSseEvents = (state, text, arrivedAt) => {
  state.buffer += text;
  while (true) {
    const boundary = state.buffer.match(/\r?\n\r?\n/);
    if (!boundary || boundary.index === undefined) return;
    const event = state.buffer.slice(0, boundary.index);
    state.buffer = state.buffer.slice(boundary.index + boundary[0].length);
    const data = event
      .split(/\r?\n/)
      .filter((line) => line.startsWith('data:'))
      .map((line) => line.slice(5).trimStart())
      .join('\n');
    if (!data || data === '[DONE]') continue;
    const needsContent = state.firstTokenAt === null;
    const mayContainUsage =
      data.includes('"usage"') ||
      data.includes('"usageMetadata"') ||
      data.includes('response.completed');
    if (!needsContent && !mayContainUsage) continue;
    try {
      const payload = JSON.parse(data);
      if (needsContent && hasContentDelta(payload)) state.firstTokenAt = arrivedAt;
      const completionTokens = completionTokensFromPayload(payload);
      if (completionTokens !== null) state.completionTokens = completionTokens;
    } catch {
      // A valid non-JSON SSE event is not a supported content delta.
    }
  }
};

const executeRequest = () =>
  new Promise((resolve) => {
    const started = performance.now();
    let firstByteAt = null;
    let responseBytes = 0;
    let responseTail = '';
    let responsePreview = '';
    const sse = { buffer: '', firstTokenAt: null, completionTokens: null };
    let settled = false;

    const finish = (result) => {
      if (settled) return;
      settled = true;
      resolve(result);
    };

    const request = transport.request(
      url,
      {
        method: 'POST',
        agent,
        headers: {
          authorization: `Bearer ${token}`,
          'content-type': 'application/json',
          accept: stream ? 'text/event-stream' : 'application/json',
          'content-length': payload.length,
        },
      },
      (response) => {
        response.on('data', (chunk) => {
          const arrivedAt = performance.now();
          if (firstByteAt === null && chunk.length > 0) firstByteAt = arrivedAt;
          responseBytes += chunk.length;
          const text = chunk.toString('utf8');
          if (responsePreview.length < 256) {
            responsePreview = (responsePreview + text).slice(0, 256);
          }
          if (!stream) {
            // Non-stream usage is normally near the end of the JSON body.
            responseTail += text;
            if (responseTail.length > responseTailLimit) {
              responseTail = responseTail.slice(-responseTailLimit);
            }
          }
          if (stream) consumeSseEvents(sse, text, arrivedAt);
        });
        response.on('end', () => {
          const ended = performance.now();
          const httpOk = response.statusCode >= 200 && response.statusCode < 300;
          const completionTokens = httpOk
            ? stream
              ? sse.completionTokens
              : completionTokensFromBody(responseTail)
            : null;
          const tokenObserved = !stream || sse.firstTokenAt !== null;
          const ok = httpOk && completionTokens !== null && tokenObserved;
          const tokenAt = stream ? sse.firstTokenAt : firstByteAt;
          finish({
            ok,
            status: response.statusCode,
            latencyMs: ended - started,
            firstBodyByteMs: firstByteAt === null ? null : firstByteAt - started,
            ttftMs: tokenAt === null ? null : tokenAt - started,
            responseBytes,
            completionTokens: completionTokens ?? 0,
            error: ok
              ? null
              : httpOk
                ? completionTokens === null
                  ? 'successful response missing completion/output token usage'
                  : 'successful SSE response missing a content token delta'
                : `HTTP ${response.statusCode}: ${responsePreview}`,
          });
        });
        response.on('error', (error) => finish({ ok: false, error: String(error) }));
      },
    );
    request.setTimeout(timeoutMs, () => request.destroy(new Error(`timeout after ${timeoutMs}ms`)));
    request.on('error', (error) => finish({ ok: false, error: String(error) }));
    request.end(payload);
  });

const runFixedCount = async (count, workers) => {
  const results = [];
  let next = 0;
  await Promise.all(
    Array.from({ length: Math.min(workers, count) }, async () => {
      while (true) {
        const index = next;
        next += 1;
        if (index >= count) return;
        results.push(await executeRequest());
      }
    }),
  );
  return results;
};

const runMeasured = async () => {
  if (durationSeconds <= 0) return runFixedCount(requestLimit, concurrency);
  const results = [];
  const deadline = performance.now() + durationSeconds * 1000;
  await Promise.all(
    Array.from({ length: concurrency }, async () => {
      while (performance.now() < deadline) results.push(await executeRequest());
    }),
  );
  return results;
};

if (warmup > 0) await runFixedCount(warmup, concurrency);
const started = performance.now();
const results = await runMeasured();
const elapsedSeconds = (performance.now() - started) / 1000;
agent.destroy();

const successful = results.filter((result) => result.ok);
const failed = results.filter((result) => !result.ok);
const latencies = successful.map((result) => result.latencyMs);
const ttfts = successful.map((result) => result.ttftMs).filter((value) => value !== null);
const firstBodyBytes = successful
  .map((result) => result.firstBodyByteMs)
  .filter((value) => value !== null);
const totalCompletionTokens = successful.reduce(
  (total, result) => total + result.completionTokens,
  0,
);
const totalResponseBytes = successful.reduce((total, result) => total + result.responseBytes, 0);

const summary = {
  url: url.toString(),
  stream,
  concurrency,
  elapsed_seconds: Number(elapsedSeconds.toFixed(3)),
  requests: results.length,
  successful: successful.length,
  failed: failed.length,
  error_rate: Number((failed.length / Math.max(1, results.length)).toFixed(6)),
  requests_per_second: Number((successful.length / elapsedSeconds).toFixed(3)),
  completion_tokens: totalCompletionTokens,
  completion_tokens_per_second: Number((totalCompletionTokens / elapsedSeconds).toFixed(3)),
  response_megabytes_per_second: Number(
    (totalResponseBytes / 1024 / 1024 / elapsedSeconds).toFixed(3),
  ),
  latency_ms: {
    p50: percentile(latencies, 0.5),
    p95: percentile(latencies, 0.95),
    p99: percentile(latencies, 0.99),
    max:
      latencies.length > 0
        ? Number(latencies.reduce((maximum, value) => Math.max(maximum, value), 0).toFixed(3))
        : null,
  },
  first_body_byte_ms: {
    p50: percentile(firstBodyBytes, 0.5),
    p95: percentile(firstBodyBytes, 0.95),
    p99: percentile(firstBodyBytes, 0.99),
  },
  ttft_ms: {
    p50: percentile(ttfts, 0.5),
    p95: percentile(ttfts, 0.95),
    p99: percentile(ttfts, 0.99),
  },
  sample_errors: failed.slice(0, 5).map((result) => result.error),
};

if (jsonOnly) {
  process.stdout.write(`${JSON.stringify(summary)}\n`);
} else {
  process.stdout.write(`${JSON.stringify(summary, null, 2)}\n`);
}

if (failed.length > 0) process.exitCode = 2;
