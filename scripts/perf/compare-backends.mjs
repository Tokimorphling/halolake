#!/usr/bin/env node

import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const argv = process.argv.slice(2);
const targets = [];
const forwarded = [];
let iterations = 3;
for (let index = 0; index < argv.length; index += 1) {
  if (argv[index] === '--target') {
    const [name, ...urlParts] = (argv[index + 1] ?? '').split('=');
    if (!name || urlParts.length === 0) throw new Error('--target requires name=url');
    targets.push({ name, url: urlParts.join('=') });
    index += 1;
  } else if (argv[index] === '--iterations') {
    iterations = Math.max(1, Number.parseInt(argv[index + 1] ?? '3', 10));
    index += 1;
  } else {
    forwarded.push(argv[index]);
  }
}
if (targets.length === 0) {
  throw new Error('provide at least one --target name=http://host/v1/chat/completions');
}

const directory = path.dirname(fileURLToPath(import.meta.url));
const loadScript = path.join(directory, 'llm-load.mjs');
const median = (values) => {
  const sorted = [...values].sort((left, right) => left - right);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 0 ? (sorted[middle - 1] + sorted[middle]) / 2 : sorted[middle];
};

const runsByTarget = new Map(targets.map((target) => [target.name, []]));
for (let iteration = 0; iteration < iterations; iteration += 1) {
  // Rotate target order between rounds to reduce thermal/cache/time drift
  // without introducing a non-reproducible random shuffle.
  const offset = iteration % targets.length;
  const roundTargets = [...targets.slice(offset), ...targets.slice(0, offset)];
  for (const target of roundTargets) {
    const child = spawnSync(
      process.execPath,
      [loadScript, '--url', target.url, '--json', ...forwarded],
      { encoding: 'utf8', env: process.env },
    );
    const line = child.stdout.trim().split('\n').filter(Boolean).at(-1);
    if (!line) throw new Error(`${target.name} produced no result: ${child.stderr}`);
    const result = JSON.parse(line);
    if (child.status !== 0 || result.failed > 0) {
      throw new Error(`${target.name} failed: ${JSON.stringify(result.sample_errors)}`);
    }
    runsByTarget.get(target.name).push(result);
    process.stderr.write(
      `benchmark_run ${JSON.stringify({ target: target.name, iteration: iteration + 1, result })}\n`,
    );
  }
}

const rows = targets.map((target) => {
  const runs = runsByTarget.get(target.name);
  return {
    name: target.name,
    rps: median(runs.map((run) => run.requests_per_second)),
    tokens: median(runs.map((run) => run.completion_tokens_per_second)),
    ttftP95: median(runs.map((run) => run.ttft_ms.p95)),
    latencyP99Worst: Math.max(...runs.map((run) => run.latency_ms.p99)),
    errorRate: median(runs.map((run) => run.error_rate)),
  };
});

process.stdout.write('| backend | req/s median | completion token/s median | TTFT p95 median ms | latency p99 worst ms | error median |\n');
process.stdout.write('|---|---:|---:|---:|---:|---:|\n');
for (const row of rows) {
  process.stdout.write(
    `| ${row.name} | ${row.rps.toFixed(2)} | ${row.tokens.toFixed(2)} | ${row.ttftP95.toFixed(2)} | ${row.latencyP99Worst.toFixed(2)} | ${(row.errorRate * 100).toFixed(3)}% |\n`,
  );
}
