import { useCallback, useEffect, useRef, useState } from 'react';
import { listCharacters } from '../../../lib/eros/client.ts';
import {
  MULTI_CONVERSATION_STORAGE_KEY,
  actorTypeOf,
  isSystemActor,
  loadMultiConversation,
  loadMultiHistory,
  sendFanOut,
  sendToActor,
  startMultiChat,
} from '../../../lib/eros/multiActor.ts';
import type {
  ErosCharacterSummary,
} from '../../../lib/eros/types.ts';
import type {
  ErosActorType,
  ErosMultiConversation,
  ErosMultiFrame,
  ErosMultiHistoryResponse,
} from '../../../lib/eros/multiActor.ts';
import type { Message } from '../../../lib/types.ts';
import { ChatComposer } from '../ChatComposer.tsx';
import { MessageItem } from '../MessageItem.tsx';
import {
  ACCENT, F, FM, RAIL, SIDEBAR_BG, SURFACE, TEXT, TEXT_FAINT, TEXT_MUTED, TEXT_SOFT,
} from '../theme.ts';

const COLUMN_MAX = 760;
const MAX_ACTORS = 3;
const FRIENDLY_ERROR = '连接失败，请重试。';
/** Two rows that say the same thing this close together are one fan-out. */
const FAN_OUT_WINDOW_MS = 180_000;

type BootPhase = 'booting' | 'picking' | 'ready' | 'error';

/** One rendered row. Assistant rows always know which actor they came from. */
interface MultiMessage extends Message {
  actorLabel: string;
  actorName: string;
  /**
   * `character` or `system`, straight off the frame / history row the engine
   * wrapped (P1.5 §2). The actor list is only a fallback for rows written
   * before the engine stamped it.
   */
  actorType: ErosActorType;
}

/**
 * Multi-actor conversation host.
 *
 * The single-actor chat is untouched: this is a second surface that talks to a
 * conversation (2–3 actors, one session each) instead of a session. The model
 * runs per actor exactly as it always did — this component only decides *which*
 * actor is addressed, and labels each reply with the identity the engine
 * wrapped the frames in.
 */
export function MultiActorChatApp({
  onBackToSingle,
}: {
  /** Returns to the single-character chat. Absent ⇒ the entry is hidden. */
  onBackToSingle?: () => void;
} = {}) {
  const [phase, setPhase] = useState<BootPhase>('booting');
  const [bootProblem, setBootProblem] = useState('');
  const [characters, setCharacters] = useState<ErosCharacterSummary[]>([]);
  const [selected, setSelected] = useState<string[]>([]);
  const [conversation, setConversation] = useState<ErosMultiConversation | null>(null);
  const [messages, setMessages] = useState<MultiMessage[]>([]);
  const [input, setInput] = useState('');
  const [target, setTarget] = useState<'all' | string>('all');
  const [streaming, setStreaming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState('');
  const abortRef = useRef<AbortController | null>(null);

  const openConversation = useCallback(async (conversationId: string) => {
    const loaded = await loadMultiConversation(conversationId);
    const history = await loadMultiHistory(conversationId);
    setConversation(loaded);
    setMessages(mapHistory(history, loaded.actors));
    setTarget('all');
    window.localStorage.setItem(MULTI_CONVERSATION_STORAGE_KEY, loaded.conversation_id);
    setPhase('ready');
  }, []);

  const bootstrap = useCallback(async () => {
    setPhase('booting');
    setBootProblem('');
    setNotice('');
    try {
      const stored = window.localStorage.getItem(MULTI_CONVERSATION_STORAGE_KEY);
      if (stored) {
        try {
          await openConversation(stored);
          return;
        } catch {
          // A conversation that no longer exists (or belongs to another user)
          // drops back to the picker instead of blocking the surface.
          window.localStorage.removeItem(MULTI_CONVERSATION_STORAGE_KEY);
        }
      }
      setCharacters(await listCharacters());
      setSelected([]);
      setPhase('picking');
    } catch (err: unknown) {
      console.error('[multi] bootstrap failed', err);
      setBootProblem(err instanceof Error ? err.message : String(err));
      setPhase('error');
    }
  }, [openConversation]);

  useEffect(() => { void bootstrap(); }, [bootstrap]);

  const toggleSelected = useCallback((genomeId: string) => {
    setSelected((prev) => {
      if (prev.includes(genomeId)) return prev.filter((id) => id !== genomeId);
      if (prev.length >= MAX_ACTORS) return prev;
      return [...prev, genomeId];
    });
  }, []);

  const handleStart = useCallback(async () => {
    if (busy || selected.length < 2) return;
    setBusy(true);
    setNotice('');
    try {
      const started = await startMultiChat({ genomeIds: selected });
      await openConversation(started.conversation_id);
    } catch (err: unknown) {
      console.error('[multi] start failed', err);
      setNotice(err instanceof Error ? err.message : FRIENDLY_ERROR);
    } finally {
      setBusy(false);
    }
  }, [busy, selected, openConversation]);

  const handleNewConversation = useCallback(async () => {
    if (busy || streaming) return;
    abortRef.current?.abort();
    window.localStorage.removeItem(MULTI_CONVERSATION_STORAGE_KEY);
    setConversation(null);
    setMessages([]);
    setBusy(true);
    try {
      setCharacters(await listCharacters());
      setSelected([]);
      setPhase('picking');
    } catch (err: unknown) {
      console.error('[multi] reload characters failed', err);
      setNotice(FRIENDLY_ERROR);
    } finally {
      setBusy(false);
    }
  }, [busy, streaming]);

  const handleSend = useCallback(async () => {
    const content = input.trim();
    if (!content || streaming || !conversation) return;
    const conversationId = conversation.conversation_id;

    setInput('');
    setNotice('');
    setMessages((prev) => [
      ...prev,
      {
        id: `local-user-${Date.now()}`,
        role: 'user',
        content,
        timestamp: new Date().toISOString(),
        actorLabel: '',
        actorName: '',
        actorType: 'character',
      },
    ]);
    setStreaming(true);
    const controller = new AbortController();
    abortRef.current = controller;

    // One placeholder per responding actor, created on that actor's first frame
    // so the fan-out renders in the order the engine runs the actors.
    const placeholderIds = new Map<string, string>();
    const accumulated = new Map<string, string>();
    const ensurePlaceholder = (
      label: string,
      name: string,
      actorType: ErosActorType | undefined,
    ): string => {
      const existing = placeholderIds.get(label);
      if (existing) return existing;
      const id = `local-assistant-${label}-${Date.now()}-${Math.random().toString(36).slice(2, 7)}`;
      placeholderIds.set(label, id);
      setMessages((prev) => [
        ...prev,
        {
          id,
          role: 'assistant',
          content: '',
          timestamp: new Date().toISOString(),
          actorLabel: label,
          actorName: name || label,
          actorType: actorTypeOf(
            { actor_label: label, actor_type: actorType },
            conversation.actors,
          ),
        },
      ]);
      return id;
    };

    const onFrame = (wrapped: ErosMultiFrame) => {
      const id = ensurePlaceholder(
        wrapped.actor_label,
        wrapped.actor_name,
        wrapped.actor_type,
      );
      const inner = wrapped.frame;
      if (inner.type === 'delta') {
        const next = `${accumulated.get(id) ?? ''}${inner.content}`;
        accumulated.set(id, next);
        setMessages((prev) => prev.map((row) => (row.id === id ? { ...row, content: next } : row)));
      } else if (inner.type === 'done') {
        setMessages((prev) => prev.map((row) => (row.id === id
          ? {
              ...row,
              content: accumulated.get(id) ?? row.content,
              generationId: inner.generation_id ?? null,
              ghostFallback: Boolean(inner.ghost_fallback),
            }
          : row)));
      } else if (inner.type === 'error') {
        setNotice(inner.user_message || inner.message || FRIENDLY_ERROR);
      }
    };

    try {
      // "全部" means every *character*. The System is deliberately not in the
      // list: including its label would make every turn an explicit
      // invocation, which would override USER_CONTROLLED and the AUTO gate.
      // Addressing it is a separate, deliberate choice (its own chip).
      const labels = conversation.actors
        .filter((actor) => !isSystemActor(actor))
        .map((actor) => actor.actor_label);
      const result = target === 'all'
        ? await sendFanOut({
            conversationId,
            actorLabels: labels,
            content,
            signal: controller.signal,
            onFrame,
          })
        : await sendToActor({
            conversationId,
            actorLabel: target,
            content,
            signal: controller.signal,
            onFrame,
          });
      if (result.status === 'error' && result.error) setNotice(result.error);

      // The stored rows are the truth: they carry `output_contract`, which the
      // stream does not. Re-read them once the turn(s) settled.
      if (result.status !== 'error') {
        const history = await loadMultiHistory(conversationId).catch(() => null);
        if (history) setMessages(mapHistory(history, conversation.actors));
      }
    } catch (err: unknown) {
      console.error('[multi] turn threw', err);
      setNotice(FRIENDLY_ERROR);
    } finally {
      setStreaming(false);
      abortRef.current = null;
    }
  }, [input, streaming, conversation, target]);

  const handleStop = useCallback(() => { abortRef.current?.abort(); }, []);

  if (phase === 'booting') return <MultiBootScreen title="正在连接…" />;
  if (phase === 'error') {
    return (
      <MultiBootScreen
        title="无法连接到角色服务"
        detail={FRIENDLY_ERROR}
        onRetry={() => void bootstrap()}
        debug={bootProblem}
      />
    );
  }
  if (phase === 'picking') {
    return (
      <ActorPicker
        characters={characters}
        selected={selected}
        onToggle={toggleSelected}
        onStart={() => void handleStart()}
        onBack={onBackToSingle}
        busy={busy}
        notice={notice}
      />
    );
  }

  const actors = conversation?.actors ?? [];
  return (
    <div
      data-testid="eros-multi-app"
      style={{ height: '100vh', display: 'flex', background: SURFACE, overflow: 'hidden' }}
    >
      <aside style={{
        width: 248, flexShrink: 0, height: '100%',
        background: SIDEBAR_BG, borderRight: `1px solid ${RAIL}`,
        display: 'flex', flexDirection: 'column', minHeight: 0,
      }}>
        <div style={{ padding: '16px 14px 10px' }}>
          <div style={{ fontFamily: FM, fontSize: 8.5, letterSpacing: '0.16em', color: TEXT_FAINT }}>
            多人剧情
          </div>
          <div style={{ fontFamily: F, fontSize: 13.5, color: TEXT, marginTop: 4 }}>
            {actors.length} 个角色
          </div>
        </div>
        <div style={{ padding: '0 12px 12px', display: 'flex', flexDirection: 'column', gap: 6 }}>
          <button
            type="button"
            data-testid="eros-multi-new"
            onClick={() => void handleNewConversation()}
            disabled={busy || streaming}
            style={railButton(false)}
          >＋ 新建多人剧情</button>
          {onBackToSingle && (
            <button type="button" onClick={onBackToSingle} disabled={streaming} style={railButton(true)}>
              返回单角色
            </button>
          )}
        </div>
        <div style={{ padding: '0 17px 7px', fontFamily: FM, fontSize: 8.5, letterSpacing: '0.16em', color: TEXT_FAINT }}>
          角色
        </div>
        <div data-testid="eros-multi-actors" style={{ flex: 1, overflowY: 'auto', padding: '0 10px 14px', minHeight: 0 }}>
          {actors.map((actor) => (
            <div
              key={actor.actor_label}
              data-actor-label={actor.actor_label}
              data-actor-type={actor.actor_type ?? 'character'}
              style={{ padding: '8px 10px', marginBottom: 2 }}
            >
              <div style={{ fontFamily: F, fontSize: 12.5, color: TEXT_SOFT }}>
                {actor.actor_name}
                {isSystemActor(actor) && (
                  <span
                    data-testid={`eros-multi-actor-system-${actor.actor_label}`}
                    style={{
                      marginLeft: 6, fontFamily: FM, fontSize: 8.5, letterSpacing: '0.14em',
                      color: TEXT_FAINT, border: `1px solid ${RAIL}`, borderRadius: 4,
                      padding: '1px 4px',
                    }}
                  >
                    SYSTEM
                  </span>
                )}
              </div>
              <div style={{ fontFamily: FM, fontSize: 9, color: TEXT_FAINT, marginTop: 3, letterSpacing: '0.05em' }}>
                {actor.actor_label}
              </div>
            </div>
          ))}
        </div>
      </aside>

      <main style={{ flex: 1, minWidth: 0, height: '100%', display: 'flex', flexDirection: 'column', background: SURFACE }}>
        <div style={{
          flex: 1, minHeight: 0, display: 'flex', flexDirection: 'column',
          width: '100%', maxWidth: COLUMN_MAX, margin: '0 auto', padding: '0 32px',
        }}>
          <div style={{
            flexShrink: 0, padding: '18px 0 12px',
            display: 'flex', alignItems: 'baseline', gap: 10, borderBottom: `1px solid ${RAIL}`,
          }}>
            <span style={{ fontFamily: F, fontSize: 14, color: TEXT }}>
              {actors.map((actor) => actor.actor_name).join(' · ')}
            </span>
            <span style={{ fontFamily: FM, fontSize: 9, letterSpacing: '0.1em', color: TEXT_FAINT }}>
              {conversation?.conversation_id.slice(0, 8)}
            </span>
          </div>

          <div data-testid="eros-multi-scroll" style={{ flex: 1, minHeight: 0, overflowY: 'auto', padding: '14px 0 8px' }}>
            {messages.length === 0 && (
              <div style={{ textAlign: 'center', paddingTop: 60 }}>
                <div style={{ fontFamily: F, fontSize: 12.5, color: TEXT_MUTED }}>
                  说一句话，两个角色都会听到
                </div>
              </div>
            )}
            {messages.map((message) => (
              <div
                key={message.id}
                data-testid="eros-multi-message"
                data-actor-label={message.actorLabel || 'user'}
                data-actor-type={message.actorType}
              >
                {message.role !== 'user' && message.actorType === 'system' && (
                  <div
                    data-testid="eros-multi-system-badge"
                    style={{
                      fontFamily: FM, fontSize: 8.5, letterSpacing: '0.16em',
                      color: TEXT_FAINT, paddingTop: 6,
                    }}
                  >
                    SYSTEM
                  </div>
                )}
                <MessageItem
                  message={message}
                  characterName={message.actorName}
                  streaming={streaming && message.id.startsWith('local-assistant-')}
                />
              </div>
            ))}
          </div>

          {notice && (
            <div style={{
              flexShrink: 0, marginBottom: 8, fontFamily: F, fontSize: 12, color: '#FF3B30',
              background: 'rgba(255,59,48,0.05)', border: '1px solid rgba(255,59,48,0.15)',
              borderRadius: 9, padding: '8px 12px',
            }}>{notice}</div>
          )}

          <div style={{ flexShrink: 0, display: 'flex', gap: 6, paddingBottom: 2 }}>
            <TargetChip
              active={target === 'all'}
              label="全部"
              testId="eros-multi-target-all"
              onClick={() => setTarget('all')}
            />
            {actors.map((actor) => (
              <TargetChip
                key={actor.actor_label}
                active={target === actor.actor_label}
                label={actor.actor_name}
                testId={`eros-multi-target-${actor.actor_label}`}
                onClick={() => setTarget(actor.actor_label)}
              />
            ))}
          </div>

          <ChatComposer
            value={input}
            onChange={setInput}
            onSend={() => void handleSend()}
            onStop={handleStop}
            streaming={streaming}
            disabled={!conversation}
            placeholder={target === 'all' ? '对所有人说…' : `对${actorName(actors, target)}说…`}
          />
        </div>
      </main>
    </div>
  );
}

function actorName(actors: { actor_label: string; actor_name: string }[], label: string): string {
  return actors.find((actor) => actor.actor_label === label)?.actor_name ?? label;
}

function railButton(quiet: boolean): React.CSSProperties {
  return {
    width: '100%', height: quiet ? 33 : 35, borderRadius: 9,
    border: `1px solid ${RAIL}`,
    background: quiet ? 'transparent' : '#FFFFFF',
    color: quiet ? TEXT_SOFT : TEXT,
    fontFamily: F, fontSize: quiet ? 12 : 12.5, cursor: 'pointer',
  };
}

function TargetChip({ active, label, testId, onClick }: {
  active: boolean;
  label: string;
  testId: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      data-testid={testId}
      onClick={onClick}
      style={{
        height: 26, padding: '0 12px', borderRadius: 13,
        border: `1px solid ${active ? `${ACCENT}55` : RAIL}`,
        background: active ? 'rgba(0,113,227,0.08)' : '#FFFFFF',
        color: active ? ACCENT : TEXT_MUTED,
        fontFamily: F, fontSize: 11.5, cursor: 'pointer',
      }}
    >{label}</button>
  );
}

/**
 * Merged view rows.
 *
 * The fan-out writes the shared user line into every selected actor's session,
 * so the merged view would otherwise repeat it once per actor. Consecutive user
 * rows with the same text inside one fan-out window collapse to a single bubble.
 * Assistant rows keep their actor, which is the identity the runtime assigned —
 * never a name parsed out of the reply text.
 */
function mapHistory(
  history: ErosMultiHistoryResponse,
  actors: { actor_label: string; actor_type?: ErosActorType }[],
): MultiMessage[] {
  const rows: MultiMessage[] = [];
  let lastUser: { content: string; at: number } | null = null;
  for (const row of history.messages) {
    const at = Date.parse(row.sent_at ?? '') || 0;
    if (row.role === 'user') {
      if (lastUser && lastUser.content === row.content && Math.abs(at - lastUser.at) < FAN_OUT_WINDOW_MS) {
        continue;
      }
      lastUser = { content: row.content, at };
      rows.push({
        id: row.id,
        role: 'user',
        content: row.content,
        timestamp: row.sent_at ?? new Date().toISOString(),
        actorLabel: '',
        actorName: '',
        // A user row belongs to no chip, and is never the System.
        actorType: 'character',
        outputContract: null,
      });
      continue;
    }
      rows.push({
        id: row.id,
        role: 'assistant',
        content: row.content ?? '',
        timestamp: row.sent_at ?? new Date().toISOString(),
        actorLabel: row.actor_label,
        actorName: row.actor_name,
        // Straight off the row (P1.5 §2): no reverse lookup needed, and an old
        // row that predates the field still resolves through the list.
        actorType: actorTypeOf(row, actors),
        outputContract: row.output_contract ?? null,
      });
  }
  return rows;
}

function ActorPicker({
  characters, selected, onToggle, onStart, onBack, busy, notice,
}: {
  characters: ErosCharacterSummary[];
  selected: string[];
  onToggle: (genomeId: string) => void;
  onStart: () => void;
  onBack?: () => void;
  busy: boolean;
  notice: string;
}) {
  const canStart = selected.length >= 2 && selected.length <= MAX_ACTORS && !busy;
  return (
    <div
      data-testid="eros-multi-picker"
      style={{
        height: '100vh', background: SURFACE, display: 'flex',
        alignItems: 'center', justifyContent: 'center', fontFamily: F,
      }}
    >
      <div style={{ width: 460, padding: 24 }}>
        <div style={{ fontSize: 15, color: TEXT, marginBottom: 6 }}>选择 2–3 个角色</div>
        <div style={{ fontSize: 12, color: TEXT_MUTED, marginBottom: 18 }}>
          每个角色拥有独立的会话、记忆与表达方式。
        </div>
        <div style={{ maxHeight: 320, overflowY: 'auto', border: `1px solid ${RAIL}`, borderRadius: 12 }}>
          {characters.length === 0 && (
            <div style={{ padding: 16, fontSize: 12, color: TEXT_MUTED }}>还没有角色，请先创建。</div>
          )}
          {characters.map((character) => {
            const checked = selected.includes(character.genome_id);
            return (
              <button
                key={character.genome_id}
                type="button"
                data-testid={`eros-multi-pick-${character.genome_id}`}
                onClick={() => onToggle(character.genome_id)}
                style={{
                  width: '100%', textAlign: 'left', padding: '10px 14px',
                  background: checked ? 'rgba(0,113,227,0.06)' : '#FFFFFF',
                  border: 'none', borderBottom: `1px solid ${RAIL}`,
                  color: checked ? ACCENT : TEXT, fontSize: 13, cursor: 'pointer',
                }}
              >
                {checked ? '✓ ' : ''}{character.name}
              </button>
            );
          })}
        </div>
        {notice && (
          <div style={{ marginTop: 12, fontSize: 12, color: '#FF3B30' }}>{notice}</div>
        )}
        <div style={{ display: 'flex', gap: 8, marginTop: 18 }}>
          <button
            type="button"
            data-testid="eros-multi-start"
            onClick={onStart}
            disabled={!canStart}
            style={{
              height: 34, padding: '0 18px', borderRadius: 9, border: 'none',
              background: canStart ? ACCENT : '#E5E5EA',
              color: canStart ? '#FFFFFF' : TEXT_FAINT,
              fontFamily: F, fontSize: 12.5, cursor: canStart ? 'pointer' : 'default',
            }}
          >开始多人剧情</button>
          {onBack && (
            <button
              type="button"
              onClick={onBack}
              style={{
                height: 34, padding: '0 18px', borderRadius: 9,
                border: `1px solid ${RAIL}`, background: 'transparent',
                color: TEXT_SOFT, fontFamily: F, fontSize: 12.5, cursor: 'pointer',
              }}
            >返回</button>
          )}
        </div>
      </div>
    </div>
  );
}

function MultiBootScreen({ title, detail, onRetry, debug }: {
  title: string;
  detail?: string;
  onRetry?: () => void;
  debug?: string;
}) {
  useEffect(() => {
    if (debug) console.error('[multi] boot detail', debug);
  }, [debug]);
  return (
    <div style={{
      height: '100vh', display: 'flex', alignItems: 'center', justifyContent: 'center',
      background: SURFACE, fontFamily: F,
    }}>
      <div style={{ textAlign: 'center', maxWidth: 360 }}>
        <div style={{ fontSize: 13.5, color: TEXT }}>{title}</div>
        {detail && <div style={{ fontSize: 12.5, color: TEXT_MUTED, marginTop: 10 }}>{detail}</div>}
        {onRetry && (
          <button
            type="button"
            onClick={onRetry}
            style={{
              marginTop: 18, height: 32, padding: '0 20px', borderRadius: 9,
              border: `1px solid ${RAIL}`, background: '#FFFFFF',
              color: ACCENT, fontFamily: F, fontSize: 12.5, cursor: 'pointer',
            }}
          >重试</button>
        )}
      </div>
    </div>
  );
}
