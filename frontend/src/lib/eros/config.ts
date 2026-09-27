/**
 * EROS adapter configuration — Phase 1 dev wiring.
 *
 * `EROS_BASE_URL` defaults to the same-origin `/eros` prefix, which the Vite dev
 * server proxies to the engine (see `vite.config.ts`). Going through the proxy
 * keeps the browser on one origin: the engine ships no CORS layer, and Phase 1
 * deliberately does not add one.
 *
 * The dev token is minted by `dev/eros-dev-token-server.mjs`, a local-only BFF
 * that reads `SUPABASE_JWT_SECRET` from the engine's own `.env`. The secret never
 * reaches the bundle; only a short-lived signed token does.
 */
const env = import.meta.env;

export const EROS_BASE_URL: string = env.VITE_EROS_BASE_URL ?? '/eros';

export const EROS_DEV_TOKEN_URL: string =
  env.VITE_EROS_DEV_TOKEN_URL ?? 'http://127.0.0.1:8787/dev/token';

/** Dev identity (`sub` claim). The engine keys every session to this user. */
export const EROS_DEV_USER_ID: string =
  env.VITE_EROS_DEV_USER_ID ?? '00000000-0000-4000-8000-000000000001';

/**
 * V1 front-end state cache only — the engine owns sessions. Nothing but the id
 * is kept locally; conversation history is always re-read from EROS.
 */
export const EROS_SESSION_STORAGE_KEY = 'eros_session_id';

/** How long a minted dev token is reused before asking the BFF again (ms). */
export const EROS_TOKEN_TTL_MS = 10 * 60 * 1000;
