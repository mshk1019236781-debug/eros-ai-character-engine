import { useEffect, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import type { Message } from '../../lib/types.ts';
import { MessageItem } from './MessageItem.tsx';
import type { VariantActions } from './MessageItem.tsx';
import { ACCENT, F, FM, TEXT_FAINT, RAIL } from './theme.ts';

const NEAR_BOTTOM_PX = 90;

/**
 * Scrolling message flow.
 *
 * New content follows the bottom only while the reader is already there: once
 * someone scrolls up to re-read, arriving deltas no longer yank the view down,
 * and a small "back to bottom" affordance appears instead.
 */
export function MessageList({
  messages,
  characterName,
  streaming,
  loading,
  emptyState,
  variantActionsFor,
}: {
  messages: Message[];
  characterName: string;
  streaming: boolean;
  loading: boolean;
  emptyState: ReactNode;
  /**
   * Per-row retry / adopt / like state. Absent for a conversation with no
   * candidates at all, which leaves every row exactly as it renders today.
   */
  variantActionsFor?: (message: Message) => VariantActions | undefined;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const stickRef = useRef(true);
  const [showJump, setShowJump] = useState(false);

  const scrollToBottom = (behavior: ScrollBehavior) => {
    const el = scrollRef.current;
    if (!el) return;
    el.scrollTo({ top: el.scrollHeight, behavior });
  };

  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const onScroll = () => {
      const distance = el.scrollHeight - el.scrollTop - el.clientHeight;
      const nearBottom = distance <= NEAR_BOTTOM_PX;
      stickRef.current = nearBottom;
      setShowJump(!nearBottom);
    };
    el.addEventListener('scroll', onScroll, { passive: true });
    onScroll();
    return () => el.removeEventListener('scroll', onScroll);
  }, []);

  // `messages` is replaced by the host on every delta, which is exactly the
  // granularity the follow behaviour needs.
  useEffect(() => {
    if (stickRef.current) scrollToBottom('auto');
  }, [messages]);

  if (loading && messages.length === 0) {
    return (
      <div style={{ flex: 1, display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
        <span style={{ fontFamily: FM, fontSize: 11, letterSpacing: '0.12em', color: TEXT_FAINT }}>
          正在载入会话…
        </span>
      </div>
    );
  }

  if (messages.length === 0) {
    return (
      <div style={{ flex: 1, display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
        {emptyState}
      </div>
    );
  }

  return (
    <div style={{ flex: 1, position: 'relative', minHeight: 0 }}>
      <div
        ref={scrollRef}
        data-testid="eros-message-scroll"
        style={{
          height: '100%',
          overflowY: 'auto',
          padding: '4px 0 8px',
          scrollbarWidth: 'thin',
        }}
      >
        {messages.map((message, index) => (
          <MessageItem
            key={message.id}
            message={message}
            characterName={characterName}
            streaming={streaming && index === messages.length - 1}
            variantActions={variantActionsFor?.(message)}
          />
        ))}
      </div>

      {showJump && (
        <button
          data-testid="eros-jump-bottom"
          onClick={() => {
            stickRef.current = true;
            setShowJump(false);
            scrollToBottom('smooth');
          }}
          style={{
            position: 'absolute', bottom: 14, left: '50%', transform: 'translateX(-50%)',
            height: 28, padding: '0 14px', borderRadius: 14,
            border: `1px solid ${RAIL}`, background: '#FFFFFF',
            color: ACCENT, fontFamily: F, fontSize: 11.5, cursor: 'pointer',
            boxShadow: '0 2px 8px rgba(0,0,0,0.08)',
          }}
        >↓ 回到底部</button>
      )}
    </div>
  );
}
