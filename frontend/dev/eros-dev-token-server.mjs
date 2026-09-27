#!/usr/bin/env node
/**
 * Dev-only token service for the EROS frontend (Phase 1).
 *
 * Why this exists: every EROS route except `/healthz` requires a bearer JWT, the
 * validator is fail-closed, and the browser must not hold the signing secret.
 * This process is the smallest thing that satisfies both constraints — it runs
 * on loopback, reads `SUPABASE_JWT_SECRET` out of the engine's own `.env`, and
 * hands the page a short-lived token. It is not part of the product; nothing
 * outside a local dev box should run it.
 *
 * The signing scheme is the same HS256 one the repo's own smoke tooling uses
 * (`tools/character_output_contract_v1_smoke.ps1`, `New-TestJwt`).
 *
 *   node dev/eros-dev-token-server.mjs
 *
 * Env:
 *   EROS_DEV_TOKEN_PORT   listen port                (default 8787)
 *   EROS_ENGINE_ENV       path to the engine `.env`  (default ../eros-engine-main/.env)
 *   EROS_DEV_USER_ID      default `sub` claim        (default 00000000-0000-4000-8000-000000000001)
 *   EROS_DEV_TOKEN_TTL    token lifetime in seconds  (default 14400)
 */
import { createHmac } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const projectRoot = resolve(here, '..');

const PORT = Number(process.env.EROS_DEV_TOKEN_PORT ?? 8787);
const ENV_FILE = resolve(
  projectRoot,
  process.env.EROS_ENGINE_ENV ?? '../eros-engine-main/.env',
);
const DEFAULT_SUB = process.env.EROS_DEV_USER_ID ?? '00000000-0000-4000-8000-000000000001';
const TTL_SECONDS = Number(process.env.EROS_DEV_TOKEN_TTL ?? 14400);

const UUID_RE = /^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$/;

function readEnvValue(name) {
  let text;
  try {
    text = readFileSync(ENV_FILE, 'utf8');
  } catch {
    throw new Error(`cannot read engine env file: ${ENV_FILE}`);
  }
  const line = text.split(/\r?\n/).find((entry) => entry.startsWith(`${name}=`));
  if (!line) throw new Error(`${name} not found in ${ENV_FILE}`);
  let value = line.slice(name.length + 1).trim();
  if (
    (value.startsWith('"') && value.endsWith('"')) ||
    (value.startsWith("'") && value.endsWith("'"))
  ) {
    value = value.slice(1, -1);
  }
  return value;
}

const b64url = (input) =>
  Buffer.from(input).toString('base64').replace(/=+$/g, '').replace(/\+/g, '-').replace(/\//g, '_');

function mintToken(sub, secret) {
  const now = Math.floor(Date.now() / 1000);
  const header = b64url(JSON.stringify({ alg: 'HS256', typ: 'JWT' }));
  const payload = b64url(
    JSON.stringify({
      sub,
      role: 'authenticated',
      aud: 'authenticated',
      iat: now,
      exp: now + TTL_SECONDS,
    }),
  );
  const unsigned = `${header}.${payload}`;
  const signature = b64url(createHmac('sha256', secret).update(unsigned).digest());
  return { token: `${unsigned}.${signature}`, expiresAt: now + TTL_SECONDS };
}

let secret;
try {
  secret = readEnvValue('SUPABASE_JWT_SECRET');
} catch (error) {
  console.error(`[eros-dev-token] ${error.message}`);
  console.error('[eros-dev-token] point EROS_ENGINE_ENV at the engine .env and retry.');
  process.exit(1);
}

function reply(res, status, body) {
  const payload = JSON.stringify(body);
  res.writeHead(status, {
    'Content-Type': 'application/json; charset=utf-8',
    'Content-Length': Buffer.byteLength(payload),
    'Access-Control-Allow-Origin': '*',
    'Access-Control-Allow-Headers': 'Content-Type',
    'Cache-Control': 'no-store',
  });
  res.end(payload);
}

const server = createServer((req, res) => {
  if (req.method === 'OPTIONS') {
    res.writeHead(204, {
      'Access-Control-Allow-Origin': '*',
      'Access-Control-Allow-Headers': 'Content-Type',
      'Access-Control-Allow-Methods': 'GET, OPTIONS',
    });
    res.end();
    return;
  }

  const url = new URL(req.url ?? '/', `http://127.0.0.1:${PORT}`);

  if (url.pathname === '/health') {
    reply(res, 200, { ok: true, env_file: ENV_FILE, default_sub: DEFAULT_SUB });
    return;
  }

  if (url.pathname === '/dev/token') {
    const sub = url.searchParams.get('sub') ?? DEFAULT_SUB;
    if (!UUID_RE.test(sub)) {
      reply(res, 400, { error: 'sub must be a UUID' });
      return;
    }
    const { token, expiresAt } = mintToken(sub, secret);
    reply(res, 200, { token, sub, expires_at: expiresAt });
    return;
  }

  reply(res, 404, { error: 'not found' });
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`[eros-dev-token] listening on http://127.0.0.1:${PORT}`);
  console.log(`[eros-dev-token] engine env: ${ENV_FILE}`);
  console.log(`[eros-dev-token] default sub: ${DEFAULT_SUB}  (ttl ${TTL_SECONDS}s)`);
});
