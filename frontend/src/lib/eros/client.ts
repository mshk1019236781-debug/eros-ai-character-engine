import {
  EROS_BASE_URL,
  EROS_DEV_TOKEN_URL,
  EROS_DEV_USER_ID,
  EROS_TOKEN_TTL_MS,
} from './config.ts';
import { parseErosStream } from './stream.ts';
import type {
  ErosCharacter,
  ErosCharacterLibraryEntry,
  ErosCharacterLibraryResponse,
  ErosCharacterSummary,
  ErosCompileCharacterRequest,
  ErosCompileCharacterResponse,
  ErosCreateCharacterRequest,
  ErosCreateCharacterResponse,
  ErosFrame,
  ErosHistoryEntry,
  ErosHistoryResponse,
  ErosProfile,
  ErosSendError,
  ErosSendResult,
  ErosSessionListEntry,
  ErosSessionListResponse,
  ErosStartResponse,
  ErosUserPersona,
  ErosUserPersonaInput,
} from './types.ts';

// ── auth ───────────────────────────────────────────────────

let tokenCache: { token: string; mintedAt: number } | null = null;

/**
 * Minimal `authToken()`.
 *
 * The engine requires a Supabase-shaped bearer JWT on every route but `/healthz`.
 * This adapter never signs and never holds the signing secret: it asks the
 * dev-only BFF on loopback for a short-lived token (same HS256 scheme the repo's
 * own smoke tooling uses, `tools/character_output_contract_v1_smoke.ps1`).
 */
export async function authToken(
  options: { force?: boolean; userId?: string } = {},
): Promise<string> {
  const staticToken = import.meta.env.VITE_EROS_DEV_TOKEN;
  if (typeof staticToken === 'string' && staticToken.length > 0) return staticToken;

  if (!options.force && tokenCache && Date.now() - tokenCache.mintedAt < EROS_TOKEN_TTL_MS) {
    return tokenCache.token;
  }

  const userId = options.userId ?? EROS_DEV_USER_ID;
  const url = new URL(EROS_DEV_TOKEN_URL);
  url.searchParams.set('sub', userId);

  let response: Response;
  try {
    response = await fetch(url.toString(), { headers: { Accept: 'application/json' } });
  } catch {
    throw new Error(
      `Dev token service unreachable at ${EROS_DEV_TOKEN_URL}. Start it in another terminal: npm run dev:token`,
    );
  }
  if (!response.ok) {
    throw new Error(`Dev token service returned HTTP ${response.status} for ${url.pathname}`);
  }

  const body = (await response.json()) as { token?: string; access_token?: string };
  const token = body.token ?? body.access_token;
  if (!token) throw new Error('Dev token service returned no token');

  tokenCache = { token, mintedAt: Date.now() };
  return token;
}

// ── shared helpers ─────────────────────────────────────────

export function authHeaders(token: string): Record<string, string> {
  return { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` };
}

async function toErosError(response: Response, label: string): Promise<ErosSendError> {
  const status = response.status;
  let message = `${label} failed with HTTP ${status}`;
  let userMessage = message;
  let code = `http_${status}`;
  let retryable = status >= 500;

  const raw = await response.text().catch(() => '');
  if (raw.trim()) {
    try {
      const body = JSON.parse(raw) as Record<string, unknown>;
      const bodyMessage = body.user_message ?? body.message ?? body.error;
      if (typeof bodyMessage === 'string' && bodyMessage.trim()) {
        userMessage = bodyMessage;
        message = bodyMessage;
      }
      if (typeof body.code === 'string') code = body.code;
      if (typeof body.retryable === 'boolean') retryable = body.retryable;
    } catch {
      userMessage = raw.trim().slice(0, 400);
      message = userMessage;
    }
  }

  return { code, message, userMessage, retryable, upstreamStatus: status };
}

async function getJson<T>(path: string, token: string, label: string): Promise<T> {
  const response = await fetch(`${EROS_BASE_URL}${path}`, { headers: authHeaders(token) });
  if (!response.ok) throw await toErosError(response, label);
  return (await response.json()) as T;
}

// ── session ────────────────────────────────────────────────

export interface CreateSessionOptions {
  /** Explicit persona instance. Takes precedence over `genomeId`. */
  instanceId?: string;
  /** Genome to resolve (engine auto-creates the caller's instance of it). */
  genomeId?: string;
  /** Skip resume and always open a fresh session. */
  forceNew?: boolean;
  userId?: string;
}

/** `POST /comp/chat/start` — create or resume the session for this character. */
export async function createSession(options: CreateSessionOptions): Promise<ErosStartResponse> {
  const token = await authToken({ userId: options.userId });
  const body: Record<string, unknown> = {};
  if (options.instanceId) body.instance_id = options.instanceId;
  else if (options.genomeId) body.genome_id = options.genomeId;
  else throw new Error('createSession requires instanceId or genomeId');
  if (options.forceNew) body.force_new = true;

  const response = await fetch(`${EROS_BASE_URL}/comp/chat/start`, {
    method: 'POST',
    headers: authHeaders(token),
    body: JSON.stringify(body),
  });
  if (!response.ok) throw await toErosError(response, 'chat/start');
  return (await response.json()) as ErosStartResponse;
}

/** `GET /comp/chat/{session_id}/history` — oldest-first. */
export async function loadHistory(
  sessionId: string,
  options: { limit?: number } = {},
): Promise<ErosHistoryEntry[]> {
  const token = await authToken();
  const limit = options.limit ?? 200;
  const body = await getJson<ErosHistoryResponse>(
    `/comp/chat/${sessionId}/history?limit=${limit}`,
    token,
    'chat/history',
  );
  return Array.isArray(body.messages) ? body.messages : [];
}

/** `GET /comp/chat/{user_id}/sessions`. */
export async function loadSessions(userId?: string): Promise<ErosSessionListEntry[]> {
  const owner = userId ?? EROS_DEV_USER_ID;
  const token = await authToken({ userId: owner });
  const body = await getJson<ErosSessionListResponse>(
    `/comp/chat/${owner}/sessions`,
    token,
    'chat/sessions',
  );
  return Array.isArray(body.sessions) ? body.sessions : [];
}

/** `GET /comp/instance/{instance_id}/profile`. */
export async function loadCharacter(instanceId: string): Promise<ErosProfile> {
  const token = await authToken();
  return getJson<ErosProfile>(`/comp/instance/${instanceId}/profile`, token, 'instance/profile');
}

export async function healthz(): Promise<boolean> {
  try {
    const response = await fetch(`${EROS_BASE_URL}/healthz`);
    return response.ok;
  } catch {
    return false;
  }
}

// ── character builder ──────────────────────────────────────

/**
 * `POST /comp/character` — author a genome.
 *
 * The engine stores what it is given: no model summarises the background, and
 * the only derived field is a deterministic `expression_core` folded from the
 * speaking style and forbidden patterns. Returns the new `genome_id`, which is
 * what `POST /comp/chat/start` then resolves into an instance and a session.
 */
export async function createCharacter(
  body: ErosCreateCharacterRequest,
  options: { userId?: string } = {},
): Promise<ErosCreateCharacterResponse> {
  const token = await authToken({ userId: options.userId });
  const response = await fetch(`${EROS_BASE_URL}/comp/character`, {
    method: 'POST',
    headers: authHeaders(token),
    body: JSON.stringify(body),
  });
  if (!response.ok) throw await toErosError(response, 'character');
  return (await response.json()) as ErosCreateCharacterResponse;
}

/** `GET /comp/characters` — every character, newest first, one row per name. */
export async function listCharacters(
  options: { userId?: string } = {},
): Promise<ErosCharacterSummary[]> {
  const token = await authToken({ userId: options.userId });
  const body = await getJson<{ characters?: ErosCharacterSummary[] }>(
    '/comp/characters',
    token,
    'characters',
  );
  return Array.isArray(body.characters) ? body.characters : [];
}

/**
 * `GET /comp/character-library` — the caller's own characters (§三).
 *
 * Derived from `persona_instances`, so it survives a reload or a restart and is
 * the only source the sidebar consults for what exists. The global
 * `listCharacters()` catalogue cannot answer that question — it lists every
 * character in the database, not the caller's.
 */
export async function loadCharacterLibrary(
  options: { userId?: string } = {},
): Promise<ErosCharacterLibraryEntry[]> {
  const token = await authToken({ userId: options.userId });
  const body = await getJson<ErosCharacterLibraryResponse>(
    '/comp/character-library',
    token,
    'character-library',
  );
  return Array.isArray(body.characters) ? body.characters : [];
}

/**
 * `DELETE /comp/character-library/{genome_id}` — remove one character (§五).
 *
 * Soft delete server-side: the instance and its conversations are archived and
 * every memory row stays. Nothing is returned beyond success — the caller
 * refetches the library, which is what makes the removal visible and keeps the
 * client from guessing.
 */
export async function archiveCharacter(
  genomeId: string,
  options: { userId?: string } = {},
): Promise<void> {
  const token = await authToken({ userId: options.userId });
  const response = await fetch(`${EROS_BASE_URL}/comp/character-library/${genomeId}`, {
    method: 'DELETE',
    headers: authHeaders(token),
  });
  if (!response.ok) throw await toErosError(response, 'character-library/delete');
}

/**
 * `GET /comp/character-library/archived` — the archived half of the library (§六).
 *
 * Same row shape as the live list, so both lists render with one component.
 * `conversation_count` counts the conversations a restore would actually bring
 * back — the ones the character archive hid, not every archived session.
 */
export async function loadArchivedLibrary(
  options: { userId?: string } = {},
): Promise<ErosCharacterLibraryEntry[]> {
  const token = await authToken({ userId: options.userId });
  const body = await getJson<ErosCharacterLibraryResponse>(
    '/comp/character-library/archived',
    token,
    'character-library/archived',
  );
  return Array.isArray(body.characters) ? body.characters : [];
}

/**
 * `POST /comp/character-library/{genome_id}/restore` — undo one archive (§六).
 *
 * The engine re-activates the instance and revives the conversations the
 * archive hid, so the resolved value says how much history came back with the
 * character. Nothing here guesses: the caller refetches the library next.
 */
export async function restoreCharacter(
  genomeId: string,
  options: { userId?: string } = {},
): Promise<{ genome_id: string; restored: boolean; restored_sessions: number }> {
  const token = await authToken({ userId: options.userId });
  const response = await fetch(
    `${EROS_BASE_URL}/comp/character-library/${genomeId}/restore`,
    { method: 'POST', headers: authHeaders(token) },
  );
  if (!response.ok) throw await toErosError(response, 'character-library/restore');
  return (await response.json()) as {
    genome_id: string;
    restored: boolean;
    restored_sessions: number;
  };
}

/** `GET /comp/user-persona` — the user's own identity, or blanks (§八). */
export async function loadUserPersona(
  options: { userId?: string } = {},
): Promise<ErosUserPersona> {
  const token = await authToken({ userId: options.userId });
  return getJson<ErosUserPersona>('/comp/user-persona', token, 'user-persona');
}

/** `PUT /comp/user-persona` — replace all six fields in one submit. */
export async function saveUserPersona(
  persona: ErosUserPersonaInput,
  options: { userId?: string } = {},
): Promise<ErosUserPersona> {
  const token = await authToken({ userId: options.userId });
  const response = await fetch(`${EROS_BASE_URL}/comp/user-persona`, {
    method: 'PUT',
    headers: authHeaders(token),
    body: JSON.stringify(persona),
  });
  if (!response.ok) throw await toErosError(response, 'user-persona');
  return (await response.json()) as ErosUserPersona;
}

/**
 * One in-flight read per genome id (P1.5 §5).
 *
 * `React.StrictMode` mounts the chat shell twice in dev, so its bootstrap effect
 * fires twice and asked the engine for the same genome twice — the duplicate
 * `GET /comp/character/{genome_id}` in the network panel at startup. The second
 * caller now waits on the first request instead of issuing another. The entry is
 * dropped the moment the request settles, so a later read is never served a
 * stale cache — this is request collapsing, not caching.
 */
const inFlightCharacterReads = new Map<string, Promise<ErosCharacter>>();

/** `GET /comp/character/{genome_id}` — one stored genome, read-only. */
export function getCharacter(genomeId: string): Promise<ErosCharacter> {
  const existing = inFlightCharacterReads.get(genomeId);
  if (existing) return existing;

  const pending = (async () => {
    const token = await authToken();
    return getJson<ErosCharacter>(`/comp/character/${genomeId}`, token, 'character');
  })();
  inFlightCharacterReads.set(genomeId, pending);
  const release = () => {
    if (inFlightCharacterReads.get(genomeId) === pending) {
      inFlightCharacterReads.delete(genomeId);
    }
  };
  // A failed read must not be handed to the next caller either.
  pending.then(release, release);
  return pending;
}

/**
 * `POST /comp/character/compile` — one model call, one structured draft.
 *
 * The draft is handed straight back: nothing is stored server-side and no
 * character is created. The user reviews (and may edit) it in the builder, and
 * only `createCharacter` writes. A second click costs a second call.
 */
export async function compileCharacter(
  body: ErosCompileCharacterRequest,
  options: { userId?: string } = {},
): Promise<ErosCompileCharacterResponse> {
  const token = await authToken({ userId: options.userId });
  const response = await fetch(`${EROS_BASE_URL}/comp/character/compile`, {
    method: 'POST',
    headers: authHeaders(token),
    body: JSON.stringify(body),
  });
  if (!response.ok) throw await toErosError(response, 'character/compile');
  return (await response.json()) as ErosCompileCharacterResponse;
}

// ── chat turn ──────────────────────────────────────────────

export interface SendMessageOptions {
  sessionId: string;
  content: string;
  /**
   * Roleplay opening turn (§ four). The engine writes its own scene-setting
   * directive instead of taking text from here, so `content` is empty and no
   * user row appears in the transcript.
   */
  opening?: boolean;
  clientMsgId?: string;
  signal?: AbortSignal;
  /** Fired per `delta` frame with the incremental text. */
  onDelta?: (chunk: string, accumulated: string) => void;
  /** Fired for every frame, for callers that want `meta`/`done`/`final` detail. */
  onFrame?: (frame: ErosFrame) => void;
}

/**
 * `POST /comp/chat/{session_id}/message/stream`.
 *
 * Frames are dispatched by `frame.type` (`meta` / `delta` / `done` / `final` /
 * `error`), never by an OpenAI-style `choices[0].delta.content` path.
 */
export async function sendMessage(options: SendMessageOptions): Promise<ErosSendResult> {
  const token = await authToken();
  const result: ErosSendResult = { status: 'ok', text: '' };

  const response = await fetch(
    `${EROS_BASE_URL}/comp/chat/${options.sessionId}/message/stream`,
    {
      method: 'POST',
      headers: { ...authHeaders(token), Accept: 'text/event-stream' },
      body: JSON.stringify({
        content: options.content,
        client_msg_id: options.clientMsgId ?? crypto.randomUUID(),
        ...(options.opening ? { opening: true } : {}),
      }),
      signal: options.signal,
    },
  );

  // A failure before the stream opens answers with a JSON body, not SSE.
  if (!response.ok) {
    result.status = 'error';
    result.error = await toErosError(response, 'message/stream');
    return result;
  }

  try {
    await parseErosStream(response, (frame) => {
      options.onFrame?.(frame);
      switch (frame.type) {
        case 'meta':
          result.messageId = frame.message_id;
          result.model = frame.model;
          result.actionType = frame.action_type;
          break;
        case 'delta':
          result.text += frame.content;
          options.onDelta?.(frame.content, result.text);
          break;
        case 'done':
          result.messageId = frame.message_id;
          result.truncated = frame.truncated;
          result.generationId = frame.generation_id ?? null;
          result.ghostFallback = Boolean(frame.ghost_fallback);
          result.usage = frame.usage ?? null;
          break;
        case 'final':
          result.final = frame;
          break;
        case 'error':
          result.status = 'error';
          result.error = {
            code: frame.code,
            message: frame.message,
            userMessage: frame.user_message,
            retryable: frame.retryable,
            upstreamStatus: frame.upstream_status ?? null,
          };
          break;
        default:
          break;
      }
    });
  } catch (err) {
    if ((err as { name?: string } | null)?.name === 'AbortError') {
      result.status = 'aborted';
      return result;
    }
    throw err;
  }

  return result;
}
