import { useEffect, useRef } from 'react';
import { Square } from 'lucide-react';
import { ACCENT, ACCENT_HOVER, F, FM, RAIL_STRONG, TEXT, TEXT_FAINT } from './theme.ts';

const MAX_HEIGHT = 180;

/**
 * Bottom-pinned composer.
 *
 * Enter sends, Shift+Enter breaks the line, and the box grows with the text up
 * to a cap so a long paragraph cannot push the conversation off screen. While a
 * turn is streaming the send action becomes Stop.
 *
 * Stop only ends client-side consumption — real server-side cancellation is a
 * known Phase 1 P1 and is deliberately untouched here.
 */
export function ChatComposer({
  value,
  onChange,
  onSend,
  onStop,
  streaming,
  disabled,
  placeholder,
}: {
  value: string;
  onChange: (next: string) => void;
  onSend: () => void;
  onStop: () => void;
  streaming: boolean;
  disabled?: boolean;
  placeholder: string;
}) {
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const canSend = !disabled && !!value.trim() && !streaming;

  // Re-measure whenever the text changes, including the clear after a send.
  useEffect(() => {
    const el = inputRef.current;
    if (!el) return;
    el.style.height = 'auto';
    el.style.height = `${Math.min(el.scrollHeight, MAX_HEIGHT)}px`;
  }, [value]);

  useEffect(() => {
    if (!streaming && !disabled) inputRef.current?.focus();
  }, [streaming, disabled]);

  return (
    <div style={{ flexShrink: 0, padding: '10px 0 16px' }}>
      <div
        style={{
          display: 'flex', alignItems: 'flex-end', gap: 8,
          background: '#FFFFFF',
          border: `1px solid ${RAIL_STRONG}`,
          borderRadius: 17,
          padding: '10px 10px 10px 15px',
          boxShadow: '0 1px 5px rgba(0,0,0,0.045)',
          transition: 'border-color 0.15s, box-shadow 0.15s',
        }}
        onFocusCapture={(event) => {
          event.currentTarget.style.borderColor = `${ACCENT}66`;
          event.currentTarget.style.boxShadow = '0 0 0 3px rgba(0,113,227,0.08)';
        }}
        onBlurCapture={(event) => {
          event.currentTarget.style.borderColor = RAIL_STRONG;
          event.currentTarget.style.boxShadow = '0 1px 5px rgba(0,0,0,0.045)';
        }}
      >
        <textarea
          ref={inputRef}
          data-testid="eros-composer"
          value={value}
          onChange={(event) => onChange(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) {
              event.preventDefault();
              if (canSend) onSend();
            }
          }}
          placeholder={placeholder}
          disabled={disabled}
          rows={1}
          style={{
            flex: 1, resize: 'none',
            background: 'transparent', border: 'none', outline: 'none',
            color: TEXT, padding: '2px 0',
            fontSize: 14, fontFamily: F, lineHeight: 1.75,
            minHeight: 24, maxHeight: MAX_HEIGHT,
          }}
        />

        {streaming ? (
          <button
            type="button"
            data-testid="eros-stop"
            onClick={onStop}
            title="停止生成"
            style={{
              width: 32, height: 32, borderRadius: 10, flexShrink: 0,
              background: 'rgba(255,59,48,0.08)',
              border: '1px solid rgba(255,59,48,0.18)',
              color: '#FF3B30', cursor: 'pointer',
              display: 'flex', alignItems: 'center', justifyContent: 'center',
            }}
          >
            <Square style={{ width: 11, height: 11 }} />
          </button>
        ) : (
          <button
            type="button"
            data-testid="eros-send"
            onClick={onSend}
            disabled={!canSend}
            title="发送 (Enter)"
            style={{
              width: 32, height: 32, borderRadius: 10, flexShrink: 0,
              background: canSend ? ACCENT : '#E5E5EA',
              border: 'none',
              color: canSend ? '#FFFFFF' : TEXT_FAINT,
              cursor: canSend ? 'pointer' : 'default',
              display: 'flex', alignItems: 'center', justifyContent: 'center',
              fontSize: 15, transition: 'background 0.15s',
            }}
            onMouseEnter={(event) => { if (canSend) event.currentTarget.style.background = ACCENT_HOVER; }}
            onMouseLeave={(event) => { if (canSend) event.currentTarget.style.background = ACCENT; }}
          >↑</button>
        )}
      </div>

      <div style={{
        fontFamily: FM, fontSize: 9, letterSpacing: '0.1em',
        color: TEXT_FAINT, opacity: 0.7, textAlign: 'center', marginTop: 7,
      }}>ENTER 发送 · SHIFT+ENTER 换行</div>
    </div>
  );
}
