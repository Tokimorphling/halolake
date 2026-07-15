#!/usr/bin/env node

import http from 'node:http';

const integerEnv = (name, fallback) => {
  const value = Number.parseInt(process.env[name] ?? '', 10);
  return Number.isFinite(value) && value >= 0 ? value : fallback;
};

const host = process.env.MOCK_HOST ?? '127.0.0.1';
const port = integerEnv('MOCK_PORT', 18080);
const firstTokenMs = integerEnv('MOCK_FIRST_TOKEN_MS', 20);
const chunkIntervalMs = integerEnv('MOCK_CHUNK_INTERVAL_MS', 2);
const chunksPerResponse = Math.max(1, integerEnv('MOCK_CHUNKS', 64));
const tokensPerChunk = Math.max(1, integerEnv('MOCK_TOKENS_PER_CHUNK', 1));
const promptTokens = integerEnv('MOCK_PROMPT_TOKENS', 32);

const counters = {
  requests: 0,
  activeRequests: 0,
  completedRequests: 0,
  openedConnections: 0,
  activeConnections: 0,
};

const wait = (milliseconds) =>
  milliseconds > 0
    ? new Promise((resolve) => setTimeout(resolve, milliseconds))
    : Promise.resolve();

const writeChunk = (response, chunk) => {
  if (response.write(chunk)) return Promise.resolve();
  return new Promise((resolve, reject) => {
    const cleanup = () => {
      response.off('drain', onDrain);
      response.off('error', onError);
      response.off('close', onClose);
    };
    const onDrain = () => {
      cleanup();
      resolve();
    };
    const onError = (error) => {
      cleanup();
      reject(error);
    };
    const onClose = () => {
      cleanup();
      reject(new Error('client closed while mock response was backpressured'));
    };
    response.once('drain', onDrain);
    response.once('error', onError);
    response.once('close', onClose);
  });
};

const readBody = async (request) => {
  const chunks = [];
  let size = 0;
  for await (const chunk of request) {
    size += chunk.length;
    if (size > 16 * 1024 * 1024) {
      throw new Error('request body exceeds 16 MiB');
    }
    chunks.push(chunk);
  }
  return Buffer.concat(chunks);
};

const completionTokens = chunksPerResponse * tokensPerChunk;

const server = http.createServer(async (request, response) => {
  if (request.method === 'GET' && request.url === '/healthz') {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ status: 'ok' }));
    return;
  }
  if (request.method === 'GET' && request.url === '/metrics') {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify(counters));
    return;
  }
  if (request.method !== 'POST') {
    response.writeHead(404, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ error: 'not found' }));
    return;
  }

  counters.requests += 1;
  counters.activeRequests += 1;
  try {
    const rawBody = await readBody(request);
    const payload = JSON.parse(rawBody.toString('utf8'));
    if (payload.stream === true) {
      response.writeHead(200, {
        'content-type': 'text/event-stream',
        'cache-control': 'no-cache',
        connection: 'keep-alive',
        'x-request-id': `mock-${counters.requests}`,
      });
      await wait(firstTokenMs);
      for (let index = 0; index < chunksPerResponse; index += 1) {
        await writeChunk(
          response,
          `data: ${JSON.stringify({
            id: 'chatcmpl-mock',
            object: 'chat.completion.chunk',
            choices: [{ index: 0, delta: { content: 'x' } }],
          })}\n\n`,
        );
        await wait(chunkIntervalMs);
      }
      await writeChunk(
        response,
        `data: ${JSON.stringify({
          id: 'chatcmpl-mock',
          object: 'chat.completion.chunk',
          choices: [],
          usage: {
            prompt_tokens: promptTokens,
            completion_tokens: completionTokens,
            total_tokens: promptTokens + completionTokens,
          },
        })}\n\n`,
      );
      response.end('data: [DONE]\n\n');
    } else {
      await wait(firstTokenMs + chunkIntervalMs * chunksPerResponse);
      response.writeHead(200, {
        'content-type': 'application/json',
        'x-request-id': `mock-${counters.requests}`,
      });
      response.end(
        JSON.stringify({
          id: 'chatcmpl-mock',
          object: 'chat.completion',
          choices: [{ index: 0, message: { role: 'assistant', content: 'x' } }],
          usage: {
            prompt_tokens: promptTokens,
            completion_tokens: completionTokens,
            total_tokens: promptTokens + completionTokens,
          },
        }),
      );
    }
    counters.completedRequests += 1;
  } catch (error) {
    if (!response.headersSent) {
      response.writeHead(400, { 'content-type': 'application/json' });
    }
    response.end(JSON.stringify({ error: String(error) }));
  } finally {
    counters.activeRequests -= 1;
  }
});

server.keepAliveTimeout = 120_000;
server.headersTimeout = 125_000;
server.requestTimeout = 0;
server.on('connection', (socket) => {
  counters.openedConnections += 1;
  counters.activeConnections += 1;
  socket.setNoDelay(true);
  socket.once('close', () => {
    counters.activeConnections -= 1;
  });
});

server.listen(port, host, () => {
  process.stdout.write(
    `${JSON.stringify({
      listening: `http://${host}:${port}`,
      firstTokenMs,
      chunkIntervalMs,
      chunksPerResponse,
      tokensPerChunk,
      completionTokens,
    })}\n`,
  );
});

const shutdown = () => server.close(() => process.exit(0));
process.on('SIGINT', shutdown);
process.on('SIGTERM', shutdown);
