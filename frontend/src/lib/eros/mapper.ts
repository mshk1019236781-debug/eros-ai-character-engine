import type { Message } from '../types.ts';
import type {
  ErosContractSegment,
  ErosHistoryEntry,
  ErosOutputContract,
  ErosScene,
} from './types.ts';

/**
 * One mapper for both channels.
 *
 * History rows and streamed turns both end up as the same front-end `Message`, so
 * the message list never has to render two different shapes.
 */
export function mapErosMessage(entry: ErosHistoryEntry): Message {
  const role = entry.role === 'assistant' ? 'assistant' : 'user';
  return {
    id: entry.id,
    role,
    content: entry.content ?? '',
    timestamp: entry.sent_at ?? entry.created_at ?? new Date().toISOString(),
    outputContract: entry.output_contract ?? null,
    generationId: null,
    ghostFallback: false,
    userMessageId: entry.user_message_id ?? null,
    opening: entry.opening,
  };
}

/**
 * The engine writes one anchor row per roleplay opening scene and marks it
 * `metadata.opening`. It exists so the turn has something to answer; it is not a
 * line the user typed, so it is dropped before the transcript is drawn.
 */
export function isHiddenHistoryRow(entry: ErosHistoryEntry): boolean {
  return entry.opening === true;
}

/**
 * P0 rule: a reply that exists must render.
 *
 * The contract is optional on the wire. When it is absent — or carries no usable
 * segments — the raw body is the body. Never an empty bubble.
 */
export function hasRenderableContract(message: Message): boolean {
  return contractSegments(message.outputContract).length > 0;
}

export function contractSegments(
  contract: ErosOutputContract | null | undefined,
): ErosContractSegment[] {
  const segments = contract?.content;
  if (!Array.isArray(segments)) return [];
  return segments.filter(
    (segment): segment is ErosContractSegment =>
      !!segment && typeof segment.text === 'string' && segment.text.length > 0,
  );
}

/** The three rendering strategies (P1-3) — the engine's vocabulary, mirrored. */
export type ErosRenderMode = 'segments_only' | 'segments_plus_tail' | 'body_fallback';

export interface ErosRenderPlan {
  mode: ErosRenderMode;
  /** Segments to draw, in engine order. Empty when the body wins. */
  segments: ErosContractSegment[];
  /** Uncovered tail text; only non-empty in `segments_plus_tail`. */
  tail: string;
  /** Raw body text; only non-empty in `body_fallback`. */
  body: string;
}

const RENDER_MODES = ['segments_only', 'segments_plus_tail', 'body_fallback'] as const;

/**
 * P1-3: how one reply is drawn.
 *
 * Reading the segments and discarding the body was how a Markdown block or a
 * 好感度 tail disappeared: `content_matches_body === false` means the split and
 * the body disagree, and the answer is a strategy, not a choice between them.
 *
 * The engine already decided (`render_mode` / `body_tail`); the client
 * re-derives the same decision only for rows written before those fields
 * existed: segments that reproduce the body are trustworthy, segments that are
 * a prefix of it leave a tail that still has to be shown, anything else means
 * the body wins. Never "segments or nothing".
 */
export function renderPlan(
  contract: ErosOutputContract | null | undefined,
  body: string,
): ErosRenderPlan {
  const segments = contractSegments(contract);
  const text = typeof body === 'string' ? body : '';
  if (segments.length === 0) {
    return { mode: 'body_fallback', segments, tail: '', body: text };
  }

  const declared = readRenderMode(contract?.render_mode);
  if (declared === 'segments_only') {
    return { mode: declared, segments, tail: '', body: '' };
  }
  if (declared === 'segments_plus_tail') {
    const tail = firstText(contract?.body_tail, uncoveredTail(text, segments));
    // A tail that was promised but not shipped is not a licence to guess:
    // render the body rather than half a reply.
    return tail
      ? { mode: declared, segments, tail, body: '' }
      : { mode: 'body_fallback', segments, tail: '', body: text || segmentsText(segments) };
  }
  if (declared === 'body_fallback') {
    return { mode: declared, segments, tail: '', body: text || segmentsText(segments) };
  }

  if (normalizeForMatch(segmentsText(segments)) === normalizeForMatch(text)) {
    return { mode: 'segments_only', segments, tail: '', body: '' };
  }
  const tail = uncoveredTail(text, segments);
  if (tail) return { mode: 'segments_plus_tail', segments, tail, body: '' };
  return { mode: 'body_fallback', segments, tail: '', body: text || segmentsText(segments) };
}

function readRenderMode(value: unknown): ErosRenderMode | null {
  return typeof value === 'string' && (RENDER_MODES as readonly string[]).includes(value)
    ? (value as ErosRenderMode)
    : null;
}

function segmentsText(segments: ErosContractSegment[]): string {
  return segments.map((segment) => segment.text).join('');
}

function firstText(...candidates: (string | null | undefined)[]): string {
  for (const candidate of candidates) {
    if (typeof candidate === 'string' && candidate.trim()) return candidate;
  }
  return '';
}

/** The one disagreement the engine forgives: reflowed whitespace and RP quotes. */
function normalizeForMatch(value: string): string {
  return [...value].filter((character) => !isMatchIgnored(character)).join('');
}

function isMatchIgnored(character: string): boolean {
  return /\s/.test(character) || '「」『』“”"\''.includes(character);
}

/**
 * The served body with the segment text cut off its front, when the segments
 * are a faithful prefix of it; `''` otherwise. Mirrors the engine's
 * `uncovered_tail`, including its "diverged ⇒ not a tail" rule.
 */
function uncoveredTail(body: string, segments: ErosContractSegment[]): string {
  const needle = [...segmentsText(segments)].filter((character) => !isMatchIgnored(character));
  if (needle.length === 0) return '';
  const characters = [...body];
  let matched = 0;
  let consumed = 0;
  for (let index = 0; index < characters.length && matched < needle.length; index += 1) {
    const character = characters[index];
    if (isMatchIgnored(character)) continue;
    if (character !== needle[matched]) return '';
    matched += 1;
    consumed = index + 1;
  }
  if (matched !== needle.length) return '';
  const opened = openWrappers(characters.slice(0, consumed).join(''));
  return stripClosingWrappers(characters.slice(consumed).join(''), opened).trim();
}

/**
 * P1.5 §3: the wrapper a segment sits inside is not content.
 *
 * `「少逞强。」` served against the segment `少逞强。` used to leave `」` heading
 * the tail, and a fenced body whose first segment carried the opening fence
 * left the closing ``` there. Exactly the closers the covered prefix opened
 * are dropped — a tail that opens a wrapper of its own keeps it, because
 * nothing was opened for it to close. Mirrors the engine's `OpenWrappers`.
 */
function openWrappers(prefix: string): { quotes: number; fences: number } {
  let quotes = 0;
  let fences = 0;
  let rest = prefix;
  for (;;) {
    const at = rest.indexOf('`');
    if (at < 0) break;
    const run = /^`+/.exec(rest.slice(at))![0].length;
    // A single tick is inline code, not a wrapper with a pair.
    if (run >= 3) fences = fences ? 0 : 1;
    rest = rest.slice(at + run);
  }
  for (const character of prefix) {
    if (character === '「' || character === '『' || character === '“') quotes += 1;
    else if (character === '」' || character === '』' || character === '”') quotes = Math.max(0, quotes - 1);
    // Directionless: the first one opens, the next one closes.
    else if (character === '"' || character === "'") quotes = quotes === 0 ? 1 : quotes - 1;
  }
  return { quotes, fences };
}

function stripClosingWrappers(
  tail: string,
  opened: { quotes: number; fences: number },
): string {
  let remaining = tail;
  let { quotes, fences } = opened;
  for (;;) {
    const trimmed = remaining.trimStart();
    if (fences > 0 && trimmed.startsWith('```')) {
      remaining = trimmed.slice(/^`+/.exec(trimmed)![0].length);
      fences -= 1;
      continue;
    }
    const head = quotes > 0 ? trimmed[0] : undefined;
    if (head === '」' || head === '』' || head === '”' || head === '"' || head === "'") {
      remaining = trimmed.slice(1);
      quotes -= 1;
      continue;
    }
    return trimmed;
  }
}

export function sceneFromContract(
  contract: ErosOutputContract | null | undefined,
): ErosScene | null {
  const scene = contract?.scene;
  if (!scene) return null;
  const time = typeof scene.time === 'string' && scene.time ? scene.time : null;
  const location = typeof scene.location === 'string' && scene.location ? scene.location : null;
  if (!time && !location) return null;
  return { time, location };
}

export function statusCardFromContract(
  contract: ErosOutputContract | null | undefined,
): Record<string, unknown> | null {
  const card = contract?.status_card;
  if (!card || typeof card !== 'object' || Array.isArray(card)) return null;
  return card as Record<string, unknown>;
}

/** Latest assistant message carrying the given channel, newest first. */
export function latestScene(messages: Message[]): ErosScene | null {
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    const scene = sceneFromContract(messages[i].outputContract);
    if (scene) return scene;
  }
  return null;
}

export function latestStatusCard(messages: Message[]): Record<string, unknown> | null {
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    const card = statusCardFromContract(messages[i].outputContract);
    if (card) return card;
  }
  return null;
}
