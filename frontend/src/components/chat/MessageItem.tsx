import { renderPlan } from '../../lib/eros/mapper.ts';
import type { CSSProperties } from 'react';
import type { Message } from '../../lib/types.ts';
import { OutputContractRenderer } from './OutputContractRenderer.tsx';
import {
  ACCENT, F, FM, LINE_DIALOGUE, SIZE_DIALOGUE, TEXT, TEXT_FAINT, TEXT_GHOST, TEXT_MUTED, TEXT_SOFT,
} from './theme.ts';

/**
 * One conversation row.
 *
 * User turns are compact bubbles on the right. Assistant turns are a plain
 * reading flow on the left — long RP text reads badly inside a bubble, so the
 * reply is not boxed at all.
 */

/**
 * The `‹ n / 3 › · ✓ 采用此版本 · ↻ 重新生成 · 👍` row under one assistant turn.
 *
 * Absent for every turn with a single candidate and for the System actor, so
 * an ordinary conversation renders exactly as it did before this existed.
 * `status` is the *displayed* candidate's own status, not the turn's: browsing
 * a rejected candidate must not claim the turn already runs it.
 */
export interface VariantActions {
  index: number;
  /** How many candidates are loaded — what the ‹ › switcher walks. */
  count: number;
  /** The per-turn ceiling the position reads against: `‹ 1 / 6 ›` (§13). */
  ceiling: number;
  status: string;
  liked: boolean;
  canRetry: boolean;
  showLike: boolean;
  busy: boolean;
  onPrev: () => void;
  onNext: () => void;
  onSelect: () => void;
  onRetry: () => void;
  onToggleLike: () => void;
}

export function MessageItem({
  message,
  characterName,
  streaming,
  variantActions,
}: {
  message: Message;
  characterName: string;
  streaming: boolean;
  variantActions?: VariantActions;
}) {
  if (message.role === 'user') {
    return (
      <div data-row="user" style={{ display: 'flex', flexDirection: 'column', alignItems: 'flex-end', marginBottom: 18 }}>
        <div data-testid="eros-user-body" style={{
          maxWidth: '76%',
          background: ACCENT,
          color: '#FFFFFF',
          borderRadius: '16px 16px 4px 16px',
          padding: '10px 15px',
          fontSize: 13.5,
          lineHeight: 1.75,
          fontFamily: F,
          whiteSpace: 'pre-wrap',
          wordBreak: 'break-word',
        }}>{message.content}</div>
        <div style={{
          fontFamily: FM, fontSize: 9, letterSpacing: '0.05em',
          color: TEXT_GHOST, marginTop: 5,
        }}>{formatTime(message.timestamp)}</div>
      </div>
    );
  }

  // Contract first — it is the engine's own typing of the reply — but never
  // "segments or nothing" (P1-3). Reading the segments and discarding the body
  // is exactly how a Markdown block or a 好感度 tail used to disappear, so the
  // engine now names a *strategy* and `renderPlan` mirrors it: segments, or
  // segments plus the body text they did not cover, or the body itself. Some
  // text is always drawn.
  const plan = renderPlan(message.outputContract, message.content ?? '');
  const segments = plan.segments;
  const hasVisible = segments.length > 0 || !!plan.tail || !!plan.body;
  // The contract's dialogue segments carry their own `speaker` label, so the
  // row-level name is only drawn when nothing inside the reply supplies one.
  // Otherwise every turn would print the character name twice.
  const showRowLabel = !segments.some((segment) => segment.speaker);

  return (
    <div data-row="assistant" style={{ display: 'flex', flexDirection: 'column', alignItems: 'flex-start', marginBottom: 26 }}>
      {showRowLabel && (
        <div style={{
          fontFamily: FM, fontSize: 9.5, letterSpacing: '0.11em',
          color: TEXT_FAINT, marginBottom: 6, textTransform: 'uppercase',
        }}>{characterName}</div>
      )}

      <div data-testid="eros-assistant-body" style={{
        maxWidth: '100%',
        minWidth: 0,
        fontSize: SIZE_DIALOGUE,
        lineHeight: LINE_DIALOGUE,
        color: TEXT,
        fontFamily: F,
        whiteSpace: 'pre-wrap',
        wordBreak: 'break-word',
      }}>
        {segments.length > 0 ? (
          <>
            <OutputContractRenderer segments={segments} />
            {plan.tail && <UncoveredTail text={plan.tail} />}
          </>
        ) : plan.body ? (
          plan.body
        ) : streaming ? (
          <span style={{ fontFamily: FM, fontSize: 12, color: TEXT_MUTED }}>···</span>
        ) : null}

        {streaming && hasVisible && (
          <span style={{
            display: 'inline-block',
            width: 2, height: 13,
            background: TEXT_FAINT,
            marginLeft: 3,
            verticalAlign: 'text-bottom',
            animation: 'eros-caret 0.9s ease-in-out infinite',
          }} />
        )}
      </div>

      {!streaming && (
        <div style={{
          fontFamily: FM, fontSize: 9, letterSpacing: '0.05em',
          color: TEXT_GHOST, marginTop: 7,
          display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap',
        }}>
          <span>{formatTime(message.timestamp)}</span>
          {variantActions && <VariantActionRow {...variantActions} />}
        </div>
      )}
    </div>
  );
}

/**
 * The body text the contract's segments did not cover (P1-3
 * `segments_plus_tail`).
 *
 * The engine only reports a tail when it verified the segments are a faithful
 * prefix of the body, so this is genuinely the same reply continuing — an
 * extra module, a Markdown block, a 好感度 line — and it is drawn rather than
 * dropped. Deliberately plain: it has no engine-declared channel.
 */
function UncoveredTail({ text }: { text: string }) {
  return (
    <div
      data-seg="body-tail"
      style={{
        marginTop: 12,
        color: TEXT_SOFT,
        fontSize: SIZE_DIALOGUE,
        lineHeight: LINE_DIALOGUE,
        whiteSpace: 'pre-wrap',
        wordBreak: 'break-word',
      }}
    >{text}</div>
  );
}

function VariantActionRow({
  index, count, ceiling, status, liked, canRetry, showLike, busy,
  onPrev, onNext, onSelect, onRetry, onToggleLike,
}: VariantActions) {
  const adopted = status === 'selected';
  return (
    <span data-testid="eros-variant-actions" data-variant-status={status} style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
      {count > 1 && (
        <span style={{ display: 'inline-flex', alignItems: 'center', gap: 2 }}>
          <button type="button" data-testid="eros-variant-prev" onClick={onPrev} disabled={busy} style={chip} aria-label="上一个版本">‹</button>
          <span data-testid="eros-variant-position" style={{ fontFamily: FM, fontSize: 9.5, color: TEXT_MUTED, minWidth: 26, textAlign: 'center' }}>{index} / {ceiling}</span>
          <button type="button" data-testid="eros-variant-next" onClick={onNext} disabled={busy} style={chip} aria-label="下一个版本">›</button>
        </span>
      )}

      {adopted ? (
        <span data-testid="eros-variant-adopted" style={{ fontFamily: F, fontSize: 10.5, color: ACCENT }}>✓ 当前采用</span>
      ) : (
        <button type="button" data-testid="eros-variant-select" onClick={onSelect} disabled={busy} style={chip}>✓ 采用此版本</button>
      )}

      <button
        type="button"
        data-testid="eros-variant-retry"
        onClick={onRetry}
        disabled={busy || !canRetry}
        style={{ ...chip, color: canRetry ? TEXT_MUTED : TEXT_GHOST, cursor: canRetry ? 'pointer' : 'default' }}
      >↻ 重新生成</button>

      {showLike && (
        <button
          type="button"
          data-testid="eros-variant-like"
          data-liked={liked ? 'true' : 'false'}
          onClick={onToggleLike}
          disabled={busy}
          style={{ ...chip, color: liked ? ACCENT : TEXT_MUTED }}
        >{liked ? '👍 已记录为角色反馈' : '👍 符合角色'}</button>
      )}
    </span>
  );
}

const chip: CSSProperties = {
  fontFamily: F, fontSize: 10.5, color: TEXT_MUTED,
  background: 'transparent', border: 'none', padding: '1px 4px',
  borderRadius: 6, cursor: 'pointer',
};

function formatTime(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return '';
  return date.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit' });
}
