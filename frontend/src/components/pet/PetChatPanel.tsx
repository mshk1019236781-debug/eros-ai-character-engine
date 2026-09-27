import { useState, useRef, useEffect, useCallback } from 'react';
import { Square } from 'lucide-react';
import type { Character, Message } from '../../lib/types';
// Type-only imports, and neither is called any more: the reply's action and scene
// typing now arrives from the engine's output_contract instead of being guessed
// from the text on the client. They stay declared so the legacy desktop shell,
// which still passes these callbacks, keeps type-checking.
import type { PetState } from '../../lib/petActions';
import type { CompanionSceneKind } from '../../lib/sceneTypes';
import { loadHistory, sendMessage } from '../../lib/eros/client.ts';
import {
  contractSegments,
  latestScene,
  latestStatusCard,
  mapErosMessage,
} from '../../lib/eros/mapper.ts';
import type { ErosScene } from '../../lib/eros/types.ts';
import { statusFieldsFromCard } from '../../lib/eros/phase2View.ts';

const F      = `-apple-system,'PingFang SC','Microsoft YaHei',system-ui,sans-serif`;
const FM     = `'SF Mono','Roboto Mono',ui-monospace,monospace`;
const ACCENT = '#0071E3';

interface Props {
  character: Character;
  /**
   * EROS session backing this conversation. The engine owns it, not the client.
   * Absent only for the legacy desktop shell, whose chat is not wired to EROS.
   */
  sessionId?: string;
  width: number | string;
  /** Omitted when the host has no shell to return to (Phase 1). */
  onClose?: () => void;
  onStreamingChange: (streaming: boolean) => void;
  /** Legacy desktop-shell callbacks. The EROS chat chain does not call them. */
  onActionDetected?: (action: PetState) => void;
  onSceneDetected?: (scene: CompanionSceneKind) => void;
}

function formatTime(iso: string): string {
  return new Date(iso).toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit' });
}

// ── Light scene / status strip ─────────────────────────────
function SceneChip({ label, value }: { label: string; value: string }) {
  return (
    <div style={{
      display: 'flex', alignItems: 'baseline', gap: 5,
      padding: '3px 8px', borderRadius: 6,
      background: 'rgba(0,0,0,0.035)',
      border: '1px solid rgba(0,0,0,0.05)',
      maxWidth: '100%',
    }}>
      <span style={{
        fontFamily: FM, fontSize: 8.5, letterSpacing: '0.12em',
        color: '#AEAEB2', flexShrink: 0,
      }}>{label}</span>
      <span style={{
        fontFamily: F, fontSize: 11.5, color: '#3A3A3C',
        overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
      }}>{value}</span>
    </div>
  );
}

// ── Output Contract segments ───────────────────────────────
// The engine typed these; the client only chooses a visual channel. Nothing is
// re-inferred from Markdown or punctuation.
function ContractSegmentBlock({ segment }: { segment: { type: string; text: string; speaker?: string | null; label?: string | null } }) {
  if (segment.type === 'narration') {
    return (
      <div style={{
        fontStyle: 'italic',
        color: '#6E6E73',
        borderLeft: '2px solid rgba(0,0,0,0.09)',
        paddingLeft: 9,
        fontSize: 12.5,
        lineHeight: 1.65,
      }}>{segment.text}</div>
    );
  }

  if (segment.type === 'special') {
    return (
      <div style={{
        border: '1px solid rgba(0,0,0,0.09)',
        borderLeft: `3px solid ${ACCENT}`,
        borderRadius: 8,
        background: 'rgba(0,113,227,0.035)',
        padding: '7px 10px',
      }}>
        {segment.label && (
          <div style={{
            fontFamily: FM, fontSize: 8.5, letterSpacing: '0.14em',
            color: '#8E8E93', marginBottom: 3, textTransform: 'uppercase',
          }}>{segment.label}</div>
        )}
        <div style={{ fontSize: 12.5, lineHeight: 1.65 }}>{segment.text}</div>
      </div>
    );
  }

  // dialogue (and anything unrecognised) renders as normal speech.
  return (
    <div>
      {segment.speaker && (
        <div style={{
          fontFamily: FM, fontSize: 8.5, letterSpacing: '0.1em',
          color: '#8E8E93', marginBottom: 2,
        }}>{segment.speaker}</div>
      )}
      <div>{segment.text}</div>
    </div>
  );
}

// ── Message Row ────────────────────────────────────────────
function MessageRow({ message, character, isStreaming }: {
  message: Message; character: Character; isStreaming: boolean;
}) {
  const isAssistant = message.role === 'assistant';
  // Contract first; when it is missing the raw body carries the reply. A served
  // message must never render empty.
  const segments = isAssistant ? contractSegments(message.outputContract) : [];

  return (
    <div style={{
      display: 'flex',
      flexDirection: isAssistant ? 'row' : 'row-reverse',
      alignItems: 'flex-end',
      gap: 8,
      padding: '2px 0 10px',
    }}>
      {/* Avatar */}
      {isAssistant && (
        <div style={{
          width: 28, height: 28, borderRadius: '50%',
          background: '#F0F0F0',
          border: '1px solid rgba(0,0,0,0.07)',
          display: 'flex', alignItems: 'center', justifyContent: 'center',
          fontSize: 13, flexShrink: 0, fontFamily: F, fontWeight: 600, color: '#6E6E73',
        }}>
          {character.name.slice(0, 1)}
        </div>
      )}

      <div style={{ maxWidth: '80%', minWidth: 0 }}>
        {/* Sender label */}
        {isAssistant && (
          <div style={{
            fontSize: 10, fontFamily: FM,
            color: '#AEAEB2', marginBottom: 3, letterSpacing: '0.04em',
          }}>
            {character.name}
          </div>
        )}

        {/* Bubble */}
        <div style={{
          background: isAssistant ? '#F1F1F3' : ACCENT,
          color: isAssistant ? '#1D1D1F' : '#FFFFFF',
          borderRadius: isAssistant ? '4px 14px 14px 14px' : '14px 4px 14px 14px',
          padding: '10px 14px',
          fontSize: 13.5,
          lineHeight: 1.7,
          fontFamily: F,
          whiteSpace: 'pre-wrap',
          wordBreak: 'break-word',
        }}>
          {segments.length > 0 ? (
            <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
              {segments.map((segment, index) => (
                <ContractSegmentBlock key={index} segment={segment} />
              ))}
            </div>
          ) : message.content || (isStreaming
            ? <span style={{ fontFamily: FM, fontSize: 12, opacity: 0.5 }}>···</span>
            : null
          )}
          {isStreaming && message.content && (
            <span style={{
              display: 'inline-block',
              width: 2, height: 13,
              background: isAssistant ? '#6E6E73' : 'rgba(255,255,255,0.75)',
              marginLeft: 2,
              verticalAlign: 'text-bottom',
              animation: 'blink 0.9s ease-in-out infinite',
            }} />
          )}
        </div>

        {/* Timestamp */}
        <div style={{
          fontSize: 9, fontFamily: FM, color: '#C7C7CC',
          marginTop: 3,
          textAlign: isAssistant ? 'left' : 'right',
          letterSpacing: '0.04em',
        }}>
          {formatTime(message.timestamp)}
        </div>
      </div>
    </div>
  );
}

// ── Main Panel ─────────────────────────────────────────────
export function PetChatPanel({ character, sessionId, width, onClose, onStreamingChange }: Props) {
  const [messages, setMessages]   = useState<Message[]>([]);
  const [input, setInput]         = useState('');
  const [streaming, setStreaming] = useState(false);
  const [loading, setLoading]     = useState(false);
  const [error, setError]         = useState('');
  const [scene, setScene]         = useState<ErosScene | null>(null);
  const [statusCard, setStatusCard] = useState<Record<string, unknown> | null>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const inputRef  = useRef<HTMLTextAreaElement>(null);
  const abortRef  = useRef<AbortController | null>(null);

  // History is engine-owned. The client keeps no copy of the conversation: the
  // only local state is the session id, and this is the one place it is read.
  const reloadHistory = useCallback(async () => {
    if (!sessionId) return null;
    setLoading(true);
    try {
      const mapped = (await loadHistory(sessionId)).map(mapErosMessage);
      setMessages(mapped);
      setScene(latestScene(mapped));
      setStatusCard(latestStatusCard(mapped));
      return mapped;
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
      return null;
    } finally {
      setLoading(false);
    }
  }, [sessionId]);

  useEffect(() => {
    void reloadHistory();
    setTimeout(() => inputRef.current?.focus(), 80);
  }, [reloadHistory]);

  useEffect(() => {
    scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight, behavior: 'smooth' });
  }, [messages]);

  useEffect(() => { onStreamingChange(streaming); }, [streaming, onStreamingChange]);

  const handleSend = useCallback(async () => {
    if (!input.trim() || streaming) return;
    if (!sessionId) {
      setError('This panel is not bound to an EROS session.');
      return;
    }
    const content = input.trim();
    setInput('');
    setError('');

    const userMsg: Message = { id: `local-user-${Date.now()}`, role: 'user', content, timestamp: new Date().toISOString() };
    const aId = `local-assistant-${Date.now()}`;
    const aMsg: Message = { id: aId, role: 'assistant', content: '', timestamp: new Date().toISOString() };

    setMessages(prev => [...prev, userMsg, aMsg]);
    setStreaming(true);
    abortRef.current = new AbortController();

    try {
      const result = await sendMessage({
        sessionId,
        content,
        signal: abortRef.current.signal,
        onDelta: (_chunk, accumulated) => {
          setMessages(prev => prev.map(m => (m.id === aId ? { ...m, content: accumulated } : m)));
        },
      });

      if (result.status === 'error') {
        setError(result.error?.userMessage ?? 'EROS request failed');
        setMessages(prev => prev.filter(m => m.id !== aId));
        return;
      }

      // Stopping before the first token leaves no reply to show.
      if (result.status === 'aborted' && !result.text.trim()) {
        setMessages(prev => prev.filter(m => m.id !== aId));
        return;
      }

      setMessages(prev => prev.map(m => (m.id === aId
        ? {
            ...m,
            content: result.text,
            model: result.model,
            generationId: result.generationId ?? null,
            ghostFallback: Boolean(result.ghostFallback),
          }
        : m)));

      // output_contract is produced at persistence time, so the typed segments
      // only exist in history. Reconcile with the engine's own view once the
      // reply matches it; if it does not, the streamed text stays on screen
      // instead of being replaced by something emptier.
      if (result.status === 'ok') {
        const server = await loadHistory(sessionId).catch(() => null);
        if (server) {
          const mapped = server.map(mapErosMessage);
          const lastAssistant = [...mapped].reverse().find(m => m.role === 'assistant');
          if (lastAssistant && lastAssistant.content.trim() === result.text.trim()) {
            setMessages(mapped);
            setScene(latestScene(mapped));
            setStatusCard(latestStatusCard(mapped));
          }
        }
      }
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
      setMessages(prev => prev.filter(m => m.id !== aId));
    } finally {
      setStreaming(false);
    }
  }, [input, streaming, sessionId]);

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); handleSend(); }
    if (e.key === 'Escape') onClose?.();
  };

  // Reload from the engine rather than clearing a local copy: there is no local
  // copy of the conversation to clear.
  const handleReload = () => {
    setError('');
    void reloadHistory();
  };

  return (
    <div style={{
      width, height: '100%',
      display: 'flex', flexDirection: 'column',
      background: '#FFFFFF',
      borderLeft: '1px solid rgba(0,0,0,0.07)',
      overflow: 'hidden',
    }}>

      {/* ── Header ──────────────────────────────── */}
      <div style={{
        padding: '10px 14px',
        borderBottom: '1px solid rgba(0,0,0,0.06)',
        flexShrink: 0,
        display: 'flex', alignItems: 'center', justifyContent: 'space-between',
        background: '#F5F5F7',
      }}>
        <div>
          <div style={{
            fontSize: 14, fontWeight: 600, fontFamily: F,
            color: '#1D1D1F', letterSpacing: '-0.01em',
          }}>
            {character.name}
          </div>
          <div style={{
            fontSize: 10, fontFamily: FM, marginTop: 1, letterSpacing: '0.06em',
            color: streaming ? '#34C759' : '#AEAEB2',
            display: 'flex', alignItems: 'center', gap: 4,
          }}>
            <span style={{
              width: 5, height: 5, borderRadius: '50%',
              background: streaming ? '#34C759' : '#AEAEB2',
              display: 'inline-block',
            }} />
            {streaming ? '正在回复…' : character.era}
          </div>
        </div>

        <div style={{ display: 'flex', gap: 6 }}>
          {[
            { label: 'RELOAD', onClick: handleReload, muted: true },
            ...(onClose ? [{ label: '✕', onClick: onClose, muted: false }] : []),
          ].map(({ label, onClick, muted }) => (
            <button key={label} onClick={onClick} style={{
              padding: '3px 10px', height: 26,
              background: 'transparent',
              border: '1px solid rgba(0,0,0,0.1)',
              borderRadius: 6,
              color: muted ? '#C7C7CC' : '#6E6E73',
              fontFamily: FM, fontSize: 10, letterSpacing: '0.06em',
              cursor: 'pointer', transition: 'all 0.12s',
            }}
              onMouseEnter={e => { e.currentTarget.style.background = 'rgba(0,0,0,0.05)'; e.currentTarget.style.color = '#1D1D1F'; }}
              onMouseLeave={e => { e.currentTarget.style.background = 'transparent'; e.currentTarget.style.color = muted ? '#C7C7CC' : '#6E6E73'; }}
            >{label}</button>
          ))}
        </div>
      </div>

      {/* ── Scene / status strip (light) ───────────── */}
      {(scene || statusCard) && (
        <div style={{
          display: 'flex', gap: 6, flexWrap: 'wrap',
          padding: '7px 14px',
          borderBottom: '1px solid rgba(0,0,0,0.06)',
          background: '#FFFFFF', flexShrink: 0,
        }}>
          {scene?.time && <SceneChip label="TIME" value={scene.time} />}
          {scene?.location && <SceneChip label="PLACE" value={scene.location} />}
          {statusCard && (
            <SceneChip
              label="STATUS"
              value={statusFieldsFromCard(statusCard)
                .map((field) => `${field.label}: ${field.value}`)
                .join(' · ')}
            />
          )}
        </div>
      )}

      {/* ── Messages ────────────────────────────── */}
      <div ref={scrollRef} style={{
        flex: 1, overflowY: 'auto', padding: '14px 14px 6px',
        display: 'flex', flexDirection: 'column',
        scrollbarWidth: 'thin', scrollbarColor: 'rgba(0,0,0,0.08) transparent',
      }}>
        {loading && messages.length === 0 && (
          <div style={{
            fontFamily: FM, fontSize: 11, color: '#AEAEB2',
            textAlign: 'center', padding: '24px 0', letterSpacing: '0.06em',
          }}>LOADING HISTORY…</div>
        )}

        {!loading && messages.length === 0 && (
          <div style={{
            fontFamily: F, fontSize: 12.5, color: '#AEAEB2',
            textAlign: 'center', padding: '24px 12px', lineHeight: 1.7,
          }}>
            还没有对话记录。输入一条消息开始。
          </div>
        )}

        {messages.map((msg, i) => (
          <MessageRow
            key={msg.id}
            message={msg}
            character={character}
            isStreaming={streaming && i === messages.length - 1 && msg.role === 'assistant'}
          />
        ))}

        {error && (
          <div style={{
            fontSize: 12, fontFamily: FM, color: '#FF3B30',
            border: '1px solid rgba(255,59,48,0.15)',
            borderRadius: 8, padding: '8px 12px',
            background: 'rgba(255,59,48,0.04)', marginTop: 4,
          }}>
            ⚠ {error}
          </div>
        )}
      </div>

      {/* ── Input ───────────────────────────────── */}
      <div style={{
        borderTop: '1px solid rgba(0,0,0,0.06)',
        background: '#F5F5F7',
        padding: '10px 12px 10px',
        flexShrink: 0,
      }}>
        <div
          style={{
            display: 'flex', alignItems: 'flex-end', gap: 8,
            background: '#FFFFFF',
            border: '1.5px solid rgba(0,0,0,0.1)',
            borderRadius: 14,
            padding: '8px 8px 8px 14px',
            boxShadow: '0 1px 4px rgba(0,0,0,0.04)',
            transition: 'border-color 0.15s',
          }}
          onFocusCapture={e => (e.currentTarget.style.borderColor = `${ACCENT}70`)}
          onBlurCapture={e => (e.currentTarget.style.borderColor = 'rgba(0,0,0,0.1)')}
        >
          <textarea
            ref={inputRef}
            value={input}
            onChange={e => {
              setInput(e.target.value);
              e.target.style.height = 'auto';
              e.target.style.height = Math.min(e.target.scrollHeight, 110) + 'px';
            }}
            onKeyDown={handleKeyDown}
            placeholder={`向${character.name}提问…`}
            disabled={streaming}
            rows={1}
            style={{
              flex: 1, resize: 'none',
              background: 'transparent', border: 'none', outline: 'none',
              color: '#1D1D1F', padding: 0,
              fontSize: 13.5, fontFamily: F, lineHeight: 1.6,
              minHeight: 22, maxHeight: 110,
            }}
          />

          {streaming ? (
            <button
              onClick={() => abortRef.current?.abort()}
              title="中止"
              style={{
                width: 30, height: 30, borderRadius: 8, flexShrink: 0,
                background: 'rgba(255,59,48,0.08)',
                border: '1px solid rgba(255,59,48,0.18)',
                color: '#FF3B30', cursor: 'pointer',
                display: 'flex', alignItems: 'center', justifyContent: 'center',
                transition: 'background 0.12s',
              }}
              onMouseEnter={e => (e.currentTarget.style.background = 'rgba(255,59,48,0.15)')}
              onMouseLeave={e => (e.currentTarget.style.background = 'rgba(255,59,48,0.08)')}
            >
              <Square style={{ width: 11, height: 11 }} />
            </button>
          ) : (
            <button
              onClick={handleSend}
              disabled={!input.trim()}
              title="发送 (Enter)"
              style={{
                width: 30, height: 30, borderRadius: 8, flexShrink: 0,
                background: input.trim() ? ACCENT : '#E5E5EA',
                border: 'none',
                color: input.trim() ? '#FFFFFF' : '#AEAEB2',
                cursor: input.trim() ? 'pointer' : 'default',
                display: 'flex', alignItems: 'center', justifyContent: 'center',
                fontSize: 16, transition: 'background 0.15s',
              }}
              onMouseEnter={e => { if (input.trim()) e.currentTarget.style.background = '#0077ED'; }}
              onMouseLeave={e => { if (input.trim()) e.currentTarget.style.background = ACCENT; }}
            >
              ↑
            </button>
          )}
        </div>

        <div style={{
          fontSize: 9, fontFamily: FM, color: '#D1D1D6',
          textAlign: 'center', marginTop: 6, letterSpacing: '0.1em',
        }}>
          ENTER 发送 · SHIFT+ENTER 换行 · ESC 关闭
        </div>
      </div>
    </div>
  );
}
