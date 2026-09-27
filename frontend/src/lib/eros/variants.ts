/**
 * Retry / variant / feedback adapter — Model Settings + Response Feedback V1.
 *
 * Mirrors `crates/eros-engine-server/src/routes/variants.rs`. One user turn owns
 * at most three candidates; only the adopted one is the turn's official reply,
 * so the client can browse the others without any of them being "real".
 *
 * Nothing here ever sends or stores an API key: this file does not touch the
 * settings surface at all (see `settings.ts`).
 */
import { EROS_BASE_URL } from './config.ts';
import { authHeaders, authToken } from './client.ts';
import { parseErosStream } from './stream.ts';
import type { ErosSendResult } from './types.ts';

export type ErosVariantStatus = 'generated' | 'selected' | 'rejected' | (string & {});

export interface ErosVariant {
  variant_id: string;
  variant_index: number;
  status: ErosVariantStatus;
  content: string;
  generation_id?: string | null;
  assistant_message_id?: string | null;
  truncated: boolean;
  actor_label?: string | null;
  created_at: string;
}

export interface ErosVariantTurn {
  user_message_id: string;
  variant_group_id: string;
  actor_label?: string | null;
  selected_variant_id?: string | null;
  /** How many candidates this turn may still generate (0 disables retry). */
  remaining: number;
  variants: ErosVariant[];
}

export interface ErosFeedback {
  variant_id: string;
  user_message_id: string;
  variant_group_id: string;
  feedback_type: string;
  generation_id?: string | null;
  created_at: string;
}

export interface ErosSessionVariants {
  session_id: string;
  turns: ErosVariantTurn[];
  feedback: ErosFeedback[];
}

/** Every candidate set and approval in one session, for the initial render. */
export async function loadVariants(sessionId: string): Promise<ErosSessionVariants> {
  const token = await authToken();
  const response = await fetch(`${EROS_BASE_URL}/comp/chat/${sessionId}/variants`, {
    headers: authHeaders(token),
  });
  if (!response.ok) throw new Error(`variants ${response.status}`);
  return (await response.json()) as ErosSessionVariants;
}

export interface RetryVariantOptions {
  sessionId: string;
  userMessageId: string;
  signal?: AbortSignal;
  onDelta?: (chunk: string, accumulated: string) => void;
}

/**
 * Regenerate one user turn. The engine re-drives the same turn and parks the
 * result as a candidate, so this never adds a second user message and never
 * touches the official reply unless the candidate is later adopted.
 */
export async function retryVariant(options: RetryVariantOptions): Promise<ErosSendResult> {
  const token = await authToken();
  const result: ErosSendResult = { status: 'ok', text: '' };

  const response = await fetch(
    `${EROS_BASE_URL}/comp/chat/${options.sessionId}/message/${options.userMessageId}/retry`,
    {
      method: 'POST',
      headers: { ...authHeaders(token), Accept: 'text/event-stream' },
      signal: options.signal,
    },
  );

  // A refusal (409 at the six-candidate cap, 404 for an unknown turn) answers
  // with JSON, not SSE.
  if (!response.ok) {
    result.status = 'error';
    result.error = {
      code: String(response.status),
      message: `retry ${response.status}`,
      userMessage: response.status === 409 ? '这条消息的候选版本已达上限' : '无法重新生成',
      retryable: false,
    };
    return result;
  }

  try {
    await parseErosStream(response, (frame) => {
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

/** Adopt one candidate: it becomes the turn's official reply. */
export async function selectVariant(
  sessionId: string,
  variantId: string,
): Promise<{ variant_id: string; assistant_message_id: string; user_message_id: string }> {
  const token = await authToken();
  const response = await fetch(
    `${EROS_BASE_URL}/comp/chat/${sessionId}/variant/${variantId}/select`,
    { method: 'POST', headers: authHeaders(token) },
  );
  if (!response.ok) throw new Error(`select ${response.status}`);
  return (await response.json()) as {
    variant_id: string;
    assistant_message_id: string;
    user_message_id: string;
  };
}

/**
 * Record or cancel "符合角色". Evidence only: the engine stores the vote and
 * nothing on this request — no review call, no exemplar, no prompt change.
 *
 * A recorded approval is queued for the character's low-frequency review, which
 * runs later, in the background, once enough approvals have accumulated. That
 * review is what can turn an approved reply into a reusable exemplar; this call
 * never waits for it and the UI never shows its internals.
 */
export async function setVariantLike(
  sessionId: string,
  variantId: string,
  liked: boolean,
): Promise<void> {
  const token = await authToken();
  const response = await fetch(
    `${EROS_BASE_URL}/comp/chat/${sessionId}/variant/${variantId}/like`,
    {
      method: liked ? 'PUT' : 'DELETE',
      headers: { ...authHeaders(token), 'Content-Type': 'application/json' },
      body: liked ? JSON.stringify({ feedback_type: 'character_like' }) : undefined,
    },
  );
  if (!response.ok) throw new Error(`like ${response.status}`);
}
