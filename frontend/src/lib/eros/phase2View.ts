/**
 * View helpers for the Phase 2 chat shell.
 *
 * Everything here is a pure projection of engine data — no chat state is
 * invented on the client. The engine owns sessions, history and the reply
 * contract; this file only decides how a few of those fields are labelled in
 * the sidebar and how a free-form `status_card` is flattened for display.
 */
import type { ErosStatusField } from './types.ts';

const FIELD_LABELS: Record<string, string> = {
  action: '行动',
  status: '状态',
  outfit: '穿着',
  mental: '心绪',
  mood: '情绪',
  location: '位置',
  time: '时间',
  note: '备注',
};

const FIELD_ORDER = ['action', 'status', 'mental', 'mood', 'outfit', 'location', 'time', 'note'];

const UUID_RE = /^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$/;

/**
 * Flatten a `status_card` into renderable rows.
 *
 * The engine does not promise a fixed shape, so keys are read dynamically: only
 * entries with a real, non-empty value survive. An empty object yields an empty
 * array, which the caller renders as nothing at all.
 */
export function statusFieldsFromCard(
  card: unknown,
  options: StatusFieldOptions = {},
): ErosStatusField[] {
  if (!card || typeof card !== 'object' || Array.isArray(card)) return [];

  const entries = Object.entries(card as Record<string, unknown>);
  const fields: ErosStatusField[] = [];
  for (const [key, raw] of entries) {
    // P1-4 (剧情演绎): `action` is a momentary behaviour — 抬眼 / 转身 / 伸手 —
    // and the same reply already carries it as an `action` segment in the
    // reading flow. Pinning it to the top of the thread turns one moment into a
    // standing status, so in roleplay the strip keeps only what the engine's
    // contract reserved for it: status / outfit / injury / long-lived state.
    // 微信聊天 is unchanged.
    if (options.roleplay && key === 'action') continue;
    const value = stringifyStatusValue(raw);
    if (!value) continue;
    fields.push({ key, label: FIELD_LABELS[key] ?? key, value });
  }

  // Keep the familiar keys in a stable reading order; unknown keys follow in
  // the order the engine emitted them.
  fields.sort((a, b) => {
    const ai = FIELD_ORDER.indexOf(a.key);
    const bi = FIELD_ORDER.indexOf(b.key);
    if (ai === -1 && bi === -1) return 0;
    if (ai === -1) return 1;
    if (bi === -1) return -1;
    return ai - bi;
  });
  return fields;
}

export interface StatusFieldOptions {
  /** 剧情演绎: momentary `action` belongs to the reply body, not the status strip. */
  roleplay?: boolean;
}

function stringifyStatusValue(raw: unknown): string {
  if (raw === null || raw === undefined) return '';
  if (typeof raw === 'string') return raw.trim();
  if (typeof raw === 'number' || typeof raw === 'boolean') return String(raw);
  if (Array.isArray(raw)) {
    return raw
      .filter((item): item is string => typeof item === 'string' && !!item.trim())
      .map((item) => item.trim())
      .join(' · ');
  }
  return '';
}

/** `23:40 · 公寓客厅` — null parts drop out; nothing at all renders when both are null. */
export function formatScene(time: string | null, location: string | null): string {
  return [time, location].filter((part): part is string => !!part && !!part.trim()).join(' · ');
}

function pad(value: number): string {
  return value < 10 ? `0${value}` : String(value);
}

/** Short absolute stamp for the session list — `09-16 14:52`. */
export function formatSessionTime(iso?: string | null): string {
  if (!iso) return '';
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return '';
  return `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

/** `裴烬` → `裴` for the avatar placeholder. */
export function avatarInitial(name: string): string {
  return name.trim().slice(0, 1) || '·';
}

/**
 * Sidebar title fallback.
 *
 * EROS does not return a session title, so the first user line stands in. This
 * is display-only and never sent back; no model call is involved.
 */
export function deriveSessionTitle(firstUserText?: string | null): string {
  const text = (firstUserText ?? '').replace(/\s+/g, ' ').trim();
  if (!text) return '新对话';
  return text.length > 28 ? `${text.slice(0, 28)}…` : text;
}

export function isServiceText(value: string): boolean {
  const text = value.trim();
  if (!text) return true;
  if (UUID_RE.test(text)) return true;
  if (/^(null|undefined|unknown|未知时间|暂无地点)$/i.test(text)) return true;
  return false;
}
