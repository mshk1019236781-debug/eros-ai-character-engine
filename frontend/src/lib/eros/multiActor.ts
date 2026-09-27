/**
 * Multi-actor (Architecture B) adapter.
 *
 * A conversation binds 2–3 *existing* character instances, one single-actor
 * session each. Nothing about the RP runtime is re-implemented here: one actor
 * turn is the same `message/stream` turn the single-actor adapter already sends,
 * addressed through the conversation instead of a bare session id.
 *
 * The engine wraps every frame of such a turn with its actor identity —
 *
 *   { conversation_id, actor_label, actor_name, session_id, frame: ProtocolFrame }
 *
 * — and this module reads that wrapper. The inner `frame` is the untouched
 * single-actor frame, so `ErosFrame` (and every renderer built on it) applies
 * unchanged. Actor identity never comes from `output_contract.dialogue.speaker`:
 * that field is *content*, and a character is free to make it say anything.
 */
import { authToken } from './client.ts';
import { EROS_BASE_URL } from './config.ts';
import type { ErosFrame, ErosOutputContract } from './types.ts';

/** Where the open conversation id is cached, so a reload resumes it. */
export const MULTI_CONVERSATION_STORAGE_KEY = 'eros_multi_conversation_id';

/** One bound actor of a conversation. */
export interface ErosMultiActor {
  actor_label: string;
  actor_name: string;
  instance_id: string;
  session_id: string;
  /**
   * `character` or `system`. The engine derives it from the genome's
   * `art_metadata`, never from anything the client sends, so it is the same
   * value on the conversation, on every frame and in the database.
   */
  actor_type?: ErosActorType;
  is_new?: boolean;
}

export type ErosActorType = 'character' | 'system';

export interface ErosMultiConversation {
  conversation_id: string;
  user_id?: string;
  metadata?: Record<string, unknown> | null;
  actors: ErosMultiActor[];
}

/** One frame of a multi-actor turn. */
export interface ErosMultiFrame {
  conversation_id: string;
  actor_label: string;
  actor_name: string;
  session_id: string;
  actor_type?: ErosActorType;
  frame: ErosFrame;
}

export interface ErosMultiHistoryEntry {
  id: string;
  session_id: string;
  instance_id: string;
  actor_label: string;
  actor_name: string;
  /**
   * `character` or `system` (P1.5 §2). The engine derives it from the actor's
   * own binding, so a reloaded page badges the System from the row itself
   * instead of reverse-looking the label up in the actor list. Absent on rows
   * served by an older engine — the list lookup stays as the fallback.
   */
  actor_type?: ErosActorType;
  role: 'user' | 'assistant' | (string & {});
  content: string;
  sent_at?: string;
  user_message_id?: string | null;
  output_contract?: ErosOutputContract | null;
}

export interface ErosMultiHistoryResponse {
  conversation_id: string;
  actors: ErosMultiActor[];
  messages: ErosMultiHistoryEntry[];
  total: number;
}

export type MultiStreamStatus = 'ok' | 'aborted' | 'error';

export interface MultiSendResult {
  status: MultiStreamStatus;
  /** Frames the engine wrapped for this actor, in arrival order. */
  frames: ErosMultiFrame[];
  /** Concatenated `delta` text — the reply body. */
  text: string;
  error?: string;
}

/**
 * `character_a` / `character_b` / `character_c`.
 *
 * The label is the runtime identity; a display name may repeat, a label may not.
 * The engine rejects labels outside `[A-Za-z0-9_-]` (they travel in a path).
 */
export function actorLabelForIndex(index: number): string {
  return `character_${String.fromCharCode('a'.charCodeAt(0) + index)}`;
}

/**
 * The System actor's label.
 *
 * A label, not a name: display names may repeat, labels may not, and the
 * engine rejects anything outside `[A-Za-z0-9_-]`. Addressing this label is
 * the explicit invocation every control mode honours.
 */
export const SYSTEM_ACTOR_LABEL = 'system';

/** One actor as the Builder binds it: a label plus the genome behind it. */
export interface MultiActorBindingInput {
  actorLabel: string;
  genomeId: string;
}

export function isSystemActor(
  actor: { actor_type?: ErosActorType } | null | undefined,
): boolean {
  return actor?.actor_type === 'system';
}

/**
 * Which chip a row belongs to (P1.5 §2).
 *
 * The engine stamps `actor_type` onto every frame and every history row, so the
 * row is the authority — a reloaded page badges the System off the row it was
 * given. The actor list is consulted only for rows written before the type was
 * stamped; the label stays an identity, never a way to infer a type.
 *
 * The input is the wire shape (a frame or a history row) so the same rule
 * applies whichever channel the row arrived on.
 */
export function actorTypeOf(
  row: { actor_label: string; actor_type?: ErosActorType },
  actors: { actor_label: string; actor_type?: ErosActorType }[],
): ErosActorType {
  return (
    row.actor_type
    ?? actors.find((actor) => actor.actor_label === row.actor_label)?.actor_type
    ?? 'character'
  );
}

async function authHeaders(): Promise<Record<string, string>> {
  const token = await authToken();
  return { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` };
}

/** The engine's own error sentence when it sent one, the raw body otherwise. */
async function failure(response: Response, label: string): Promise<string> {
  const raw = await response.text().catch(() => '');
  if (raw.trim()) {
    try {
      const body = JSON.parse(raw) as Record<string, unknown>;
      const message = body.user_message ?? body.message ?? body.error;
      if (typeof message === 'string' && message.trim()) return message;
    } catch {
      // Not JSON — fall through to the trimmed body.
    }
    return raw.trim().slice(0, 300);
  }
  return `${label} 失败（HTTP ${response.status}）`;
}

/**
 * `POST /comp/multi-chat/start` — bind 2–3 genomes into one conversation.
 *
 * Each genome resolves into its own instance and its own fresh session (the
 * engine sets `force_new`), so a conversation never adopts a single-actor chat.
 */
export async function startMultiChat(input: {
  /** Character genomes, bound in order as `character_a`, `character_b`, … */
  genomeIds?: string[];
  /**
   * Explicit bindings, used when the conversation also carries a System actor
   * whose label is not index-shaped. Takes precedence over `genomeIds`.
   */
  actors?: MultiActorBindingInput[];
  metadata?: Record<string, unknown> | null;
  userId?: string;
}): Promise<ErosMultiConversation> {
  const token = await authToken({ userId: input.userId });
  const actors: MultiActorBindingInput[] = input.actors
    ?? (input.genomeIds ?? []).map((genomeId, index) => ({
      actorLabel: actorLabelForIndex(index),
      genomeId,
    }));
  const response = await fetch(`${EROS_BASE_URL}/comp/multi-chat/start`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
    body: JSON.stringify({
      actors: actors.map((actor) => ({
        actor_label: actor.actorLabel,
        genome_id: actor.genomeId,
      })),
      metadata: input.metadata ?? null,
    }),
  });
  if (!response.ok) throw new Error(await failure(response, 'multi-chat/start'));
  return (await response.json()) as ErosMultiConversation;
}

/** `GET /comp/multi-chat/{conversation_id}` — the conversation and its actors. */
export async function loadMultiConversation(
  conversationId: string,
): Promise<ErosMultiConversation> {
  const headers = await authHeaders();
  const response = await fetch(`${EROS_BASE_URL}/comp/multi-chat/${conversationId}`, {
    headers,
  });
  if (!response.ok) throw new Error(await failure(response, 'multi-chat'));
  return (await response.json()) as ErosMultiConversation;
}

/**
 * `GET /comp/multi-chat/{conversation_id}/history` — the merged view.
 *
 * Display only. Every row still belongs to exactly one actor session; the
 * engine never feeds this merge back to a model.
 */
export async function loadMultiHistory(
  conversationId: string,
  limit = 40,
): Promise<ErosMultiHistoryResponse> {
  const headers = await authHeaders();
  const response = await fetch(
    `${EROS_BASE_URL}/comp/multi-chat/${conversationId}/history?limit=${limit}`,
    { headers },
  );
  if (!response.ok) throw new Error(await failure(response, 'multi-chat/history'));
  return (await response.json()) as ErosMultiHistoryResponse;
}

/**
 * Read an EROS SSE body of `MultiActorFrame`s.
 *
 * Same shape as `parseErosStream` (data-only lines, comments tolerated), one
 * level up: each payload is the actor wrapper, and the inner frame is handed to
 * `onFrame` after the wrapper has been checked.
 */
export async function parseMultiActorStream(
  response: Response,
  onFrame: (frame: ErosMultiFrame) => void,
): Promise<void> {
  if (!response.body) throw new Error('EROS stream had no body');

  const reader = response.body.getReader();
  const decoder = new TextDecoder('utf-8');
  let buffer = '';

  const dispatch = (payload: string) => {
    if (!payload || payload === '[DONE]') return;
    let wrapped: ErosMultiFrame;
    try {
      wrapped = JSON.parse(payload) as ErosMultiFrame;
    } catch {
      // A frame this build cannot read is dropped rather than fatal.
      return;
    }
    if (!wrapped || typeof wrapped.actor_label !== 'string') return;
    const inner = wrapped.frame as ErosFrame | undefined;
    if (!inner || typeof inner.type !== 'string') return;
    onFrame(wrapped);
  };

  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;

    buffer += decoder.decode(value, { stream: true });
    const lines = buffer.split('\n');
    buffer = lines.pop() ?? '';

    for (const rawLine of lines) {
      const line = rawLine.endsWith('\r') ? rawLine.slice(0, -1) : rawLine;
      if (!line || line.startsWith(':')) continue;
      if (!line.startsWith('data:')) continue;
      dispatch(line.slice(5).trim());
    }
  }

  const tail = buffer.trim();
  if (tail.startsWith('data:')) dispatch(tail.slice(5).trim());
}

interface StreamOptions {
  conversationId: string;
  content: string;
  clientMsgId?: string;
  signal?: AbortSignal;
  onFrame?: (frame: ErosMultiFrame) => void;
}

/** `POST .../actor/{actor_label}/message/stream` — one explicit actor answers. */
export async function sendToActor(
  options: StreamOptions & { actorLabel: string },
): Promise<MultiSendResult> {
  const token = await authToken();
  const response = await fetch(
    `${EROS_BASE_URL}/comp/multi-chat/${options.conversationId}/actor/${options.actorLabel}/message/stream`,
    {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        Authorization: `Bearer ${token}`,
        Accept: 'text/event-stream',
      },
      body: JSON.stringify({
        content: options.content,
        client_msg_id: options.clientMsgId ?? crypto.randomUUID(),
      }),
      signal: options.signal,
    },
  );
  if (!response.ok) {
    return { status: 'error', frames: [], text: '', error: await failure(response, 'message/stream') };
  }
  return consume(response, options);
}

/**
 * `POST .../message/stream` — one user message, an ordered list of actors.
 *
 * The engine runs the actors in the order given, so the reply order on screen
 * is the order requested here; each actor's turn is delivered to its own
 * session, which is what keeps the histories private.
 */
export async function sendFanOut(
  options: StreamOptions & { actorLabels: string[] },
): Promise<MultiSendResult> {
  const token = await authToken();
  const response = await fetch(
    `${EROS_BASE_URL}/comp/multi-chat/${options.conversationId}/message/stream`,
    {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        Authorization: `Bearer ${token}`,
        Accept: 'text/event-stream',
      },
      body: JSON.stringify({
        actors: options.actorLabels,
        message: {
          content: options.content,
          client_msg_id: options.clientMsgId ?? crypto.randomUUID(),
        },
      }),
      signal: options.signal,
    },
  );
  if (!response.ok) {
    return { status: 'error', frames: [], text: '', error: await failure(response, 'message/stream') };
  }
  return consume(response, options);
}

async function consume(
  response: Response,
  options: Pick<StreamOptions, 'onFrame'>,
): Promise<MultiSendResult> {
  const result: MultiSendResult = { status: 'ok', frames: [], text: '' };
  try {
    await parseMultiActorStream(response, (wrapped) => {
      result.frames.push(wrapped);
      options.onFrame?.(wrapped);
      const inner = wrapped.frame;
      if (inner.type === 'delta') result.text += inner.content;
      if (inner.type === 'error') {
        result.status = 'error';
        result.error = inner.user_message || inner.message;
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
