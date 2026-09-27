import { useState } from 'react';
import type { ErosCharacterLibraryEntry } from '../../lib/eros/types.ts';
import {
  ACCENT, DANGER, F, FM, RAIL, SIDEBAR_BG, TEXT, TEXT_FAINT, TEXT_MUTED, TEXT_SOFT, TINT,
} from './theme.ts';

/**
 * A session as the sidebar shows it.
 *
 * `title` is derived on the client for display only — EROS returns no session
 * title, and the spec forbids spending a model call on one. `time` is the
 * engine's `last_active_at` when present.
 */
export interface SessionView {
  sessionId: string;
  title: string;
  time: string;
  isActive: boolean;
}

/** Compact action used inside a character row (delete / confirm / cancel). */
const miniButton = {
  height: 22,
  padding: '0 7px',
  borderRadius: 6,
  border: `1px solid ${RAIL}`,
  background: '#FFFFFF',
  color: TEXT_SOFT,
  fontFamily: F,
  fontSize: 11,
  cursor: 'pointer',
} as const;

/**
 * Left rail: the characters this user owns, a real new-chat action, and the
 * engine's own session list for the open character.
 *
 * `characters` is the engine's library (§三) — never a client-side guess. Each
 * row switches the active character and reopens that character's newest
 * conversation, so A → B → A restores A exactly.
 */
export function ChatSidebar({
  characterName,
  characterInitial,
  characterSubtitle,
  characters,
  activeGenomeId,
  onSelectCharacter,
  onDeleteCharacter,
  archived,
  onRestoreCharacter,
  sessions,
  onNewSession,
  onCreateCharacter,
  onOpenSettings,
  onSelectSession,
  busy,
}: {
  characterName: string;
  characterInitial: string;
  characterSubtitle?: string;
  /** Every character the user owns. Absent or empty ⇒ the section is hidden. */
  characters?: ErosCharacterLibraryEntry[];
  /** Which of them is open, so its row can be highlighted. */
  activeGenomeId?: string;
  onSelectCharacter?: (entry: ErosCharacterLibraryEntry) => void;
  /** Archive one character. Absent ⇒ the delete affordance is not rendered. */
  onDeleteCharacter?: (entry: ErosCharacterLibraryEntry) => void | Promise<void>;
  /** Characters that were archived and can be put back (§六). Empty ⇒ hidden. */
  archived?: ErosCharacterLibraryEntry[];
  /** Undo one archive. Absent ⇒ the restore affordance is not rendered. */
  onRestoreCharacter?: (entry: ErosCharacterLibraryEntry) => void | Promise<void>;
  sessions: SessionView[];
  onNewSession: () => void;
  /** Opens the Character Builder. Absent ⇒ the entry is not rendered. */
  onCreateCharacter?: () => void;
  /** Opens Model Settings. Absent ⇒ the entry is not rendered. */
  onOpenSettings?: () => void;
  onSelectSession: (sessionId: string) => void;
  busy: boolean;
}) {
  // Delete is two steps by design (§五): the first click only arms the row, and
  // the confirm sits where the row is rather than in a browser dialog the
  // desktop webview may suppress.
  const [confirmingDelete, setConfirmingDelete] = useState<string | null>(null);
  const library = characters ?? [];
  const archivedLibrary = archived ?? [];

  return (
    <aside style={{
      width: 248,
      flexShrink: 0,
      height: '100%',
      background: SIDEBAR_BG,
      borderRight: `1px solid ${RAIL}`,
      display: 'flex',
      flexDirection: 'column',
      minHeight: 0,
    }}>
      {/* Current character */}
      <div style={{ padding: '16px 14px 13px', display: 'flex', alignItems: 'center', gap: 10 }}>
        <div style={{
          width: 34, height: 34, borderRadius: 11,
          background: '#FFFFFF',
          border: `1px solid ${RAIL}`,
          display: 'flex', alignItems: 'center', justifyContent: 'center',
          fontFamily: F, fontSize: 14.5, fontWeight: 600, color: TEXT_MUTED,
          flexShrink: 0,
        }}>{characterInitial}</div>
        <div style={{ minWidth: 0 }}>
          <div style={{
            fontFamily: F, fontSize: 13.5, color: TEXT, fontWeight: 500,
            letterSpacing: '-0.01em',
            overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
          }}>{characterName}</div>
          {characterSubtitle && (
            <div style={{
              fontFamily: F, fontSize: 10.5, color: TEXT_FAINT, marginTop: 2,
              overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
            }}>{characterSubtitle}</div>
          )}
        </div>
      </div>

      {/* New chat — a real engine session, not a cleared view */}
      <div style={{ padding: '0 12px 14px', display: 'flex', flexDirection: 'column', gap: 6 }}>
        <button
          type="button"
          data-testid="eros-new-session"
          onClick={onNewSession}
          disabled={busy}
          style={{
            width: '100%', height: 35, borderRadius: 9,
            border: `1px solid ${RAIL}`,
            background: '#FFFFFF',
            color: busy ? TEXT_FAINT : TEXT,
            fontFamily: F, fontSize: 12.5,
            cursor: busy ? 'default' : 'pointer',
            display: 'flex', alignItems: 'center', justifyContent: 'center', gap: 6,
            transition: 'background 0.12s',
          }}
          onMouseEnter={(event) => { if (!busy) event.currentTarget.style.background = TINT; }}
          onMouseLeave={(event) => { event.currentTarget.style.background = '#FFFFFF'; }}
        ><span style={{ fontSize: 14, lineHeight: 1 }}>＋</span> 新对话</button>
        {onCreateCharacter && (
          <button
            type="button"
            data-testid="eros-create-character"
            onClick={onCreateCharacter}
            disabled={busy}
            style={{
              width: '100%', height: 33, borderRadius: 9,
              border: `1px solid ${RAIL}`,
              background: 'transparent',
              color: busy ? TEXT_FAINT : TEXT_SOFT,
              fontFamily: F, fontSize: 12,
              cursor: busy ? 'default' : 'pointer',
              display: 'flex', alignItems: 'center', justifyContent: 'center', gap: 6,
              transition: 'background 0.12s',
            }}
            onMouseEnter={(event) => { if (!busy) event.currentTarget.style.background = TINT; }}
            onMouseLeave={(event) => { event.currentTarget.style.background = 'transparent'; }}
          >创建角色</button>
        )}
        {onOpenSettings && (
          <button
            type="button"
            data-testid="eros-open-settings"
            onClick={onOpenSettings}
            disabled={busy}
            style={{
              width: '100%', height: 33, borderRadius: 9,
              border: `1px solid ${RAIL}`,
              background: 'transparent',
              color: busy ? TEXT_FAINT : TEXT_SOFT,
              fontFamily: F, fontSize: 12,
              cursor: busy ? 'default' : 'pointer',
              display: 'flex', alignItems: 'center', justifyContent: 'center', gap: 6,
              transition: 'background 0.12s',
            }}
            onMouseEnter={(event) => { if (!busy) event.currentTarget.style.background = TINT; }}
            onMouseLeave={(event) => { event.currentTarget.style.background = 'transparent'; }}
          >模型设置</button>
        )}
      </div>

      {library.length > 0 && (
        <>
          <div style={{
            padding: '0 17px 7px',
            fontFamily: FM, fontSize: 8.5, letterSpacing: '0.16em',
            color: TEXT_FAINT,
          }}>角色</div>

          <div
            data-testid="eros-character-list"
            style={{
              maxHeight: 200, overflowY: 'auto', flexShrink: 0,
              padding: '0 10px 10px', minHeight: 0,
            }}
          >
            {library.map((entry) => {
              const isActive = entry.genome_id === activeGenomeId;
              const confirming = confirmingDelete === entry.genome_id;
              return (
                <div
                  key={entry.genome_id}
                  data-genome-id={entry.genome_id}
                  data-active={isActive ? 'true' : 'false'}
                  style={{
                    display: 'flex', alignItems: 'center', gap: 4,
                    borderLeft: `2px solid ${isActive ? ACCENT : 'transparent'}`,
                    borderRadius: 8, marginBottom: 2, padding: '7px 6px 7px 7px',
                    background: isActive ? 'rgba(0,113,227,0.07)' : 'transparent',
                  }}
                >
                  <button
                    type="button"
                    data-testid="eros-character-select"
                    onClick={() => { setConfirmingDelete(null); onSelectCharacter?.(entry); }}
                    disabled={busy}
                    style={{
                      flex: 1, minWidth: 0, textAlign: 'left', border: 'none',
                      background: 'transparent', padding: 0,
                      cursor: busy ? 'default' : 'pointer',
                    }}
                  >
                    <div style={{
                      fontFamily: F, fontSize: 12.5,
                      color: isActive ? ACCENT : TEXT_SOFT,
                      fontWeight: isActive ? 500 : 400,
                      overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
                    }}>{entry.name}</div>
                    <div style={{
                      fontFamily: FM, fontSize: 9, color: TEXT_FAINT,
                      marginTop: 2, letterSpacing: '0.05em',
                    }}>{entry.conversation_count} 个对话</div>
                  </button>

                  {onDeleteCharacter && (confirming ? (
                    <div style={{ display: 'flex', gap: 4, flexShrink: 0 }}>
                      <button
                        type="button"
                        data-testid="eros-character-delete-confirm"
                        onClick={() => {
                          const target = entry;
                          setConfirmingDelete(null);
                          void onDeleteCharacter(target);
                        }}
                        disabled={busy}
                        style={{ ...miniButton, color: DANGER, borderColor: 'rgba(255,59,48,0.35)' }}
                      >删除</button>
                      <button
                        type="button"
                        onClick={() => setConfirmingDelete(null)}
                        disabled={busy}
                        style={miniButton}
                      >取消</button>
                    </div>
                  ) : (
                    <button
                      type="button"
                      data-testid="eros-character-delete"
                      title="删除角色"
                      onClick={() => setConfirmingDelete(entry.genome_id)}
                      disabled={busy}
                      style={{ ...miniButton, flexShrink: 0 }}
                    >🗑</button>
                  ))}
                </div>
              );
            })}
          </div>
        </>
      )}

      {archivedLibrary.length > 0 && (
        <>
          <div style={{
            padding: '0 17px 7px',
            fontFamily: FM, fontSize: 8.5, letterSpacing: '0.16em',
            color: TEXT_FAINT, display: 'flex', alignItems: 'center', gap: 6,
          }}>
            已归档
            <span style={{ letterSpacing: '0.05em' }}>{archivedLibrary.length}</span>
          </div>

          {/* §六: archiving is not a dead end. The row shows how much history a
              restore would bring back, and the button is the whole entry point —
              it calls the engine's restore, then the shell refetches. */}
          <div
            data-testid="eros-archived-list"
            style={{
              maxHeight: 140, overflowY: 'auto', flexShrink: 0,
              padding: '0 10px 10px', minHeight: 0,
            }}
          >
            {archivedLibrary.map((entry) => (
              <div
                key={entry.genome_id}
                data-genome-id={entry.genome_id}
                style={{
                  display: 'flex', alignItems: 'center', gap: 4,
                  borderRadius: 8, marginBottom: 2, padding: '7px 6px 7px 7px',
                }}
              >
                <div style={{ flex: 1, minWidth: 0 }}>
                  <div style={{
                    fontFamily: F, fontSize: 12.5, color: TEXT_MUTED,
                    overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
                  }}>{entry.name}</div>
                  <div style={{
                    fontFamily: FM, fontSize: 9, color: TEXT_FAINT,
                    marginTop: 2, letterSpacing: '0.05em',
                  }}>{entry.conversation_count} 个对话可恢复</div>
                </div>
                {onRestoreCharacter && (
                  <button
                    type="button"
                    data-testid="eros-character-restore"
                    title="恢复角色"
                    onClick={() => void onRestoreCharacter(entry)}
                    disabled={busy}
                    style={{ ...miniButton, flexShrink: 0 }}
                  >恢复</button>
                )}
              </div>
            ))}
          </div>
        </>
      )}

      <div style={{
        padding: '0 17px 7px',
        fontFamily: FM, fontSize: 8.5, letterSpacing: '0.16em',
        color: TEXT_FAINT,
      }}>历史会话</div>

      <div
        data-testid="eros-session-list"
        style={{ flex: 1, overflowY: 'auto', padding: '0 10px 14px', minHeight: 0 }}
      >
        {sessions.length === 0 && (
          <div style={{
            padding: '10px 8px', fontFamily: F, fontSize: 11.5,
            color: TEXT_FAINT, lineHeight: 1.7,
          }}>还没有会话</div>
        )}

        {sessions.map((session) => (
          <button
            key={session.sessionId}
            type="button"
            data-session-id={session.sessionId}
            data-active={session.isActive ? 'true' : 'false'}
            onClick={() => onSelectSession(session.sessionId)}
            style={{
              width: '100%', textAlign: 'left',
              border: 'none',
              borderLeft: `2px solid ${session.isActive ? ACCENT : 'transparent'}`,
              borderRadius: 8,
              background: session.isActive ? 'rgba(0,113,227,0.07)' : 'transparent',
              padding: '8px 10px 8px 9px', marginBottom: 2,
              cursor: 'pointer', display: 'block',
              transition: 'background 0.12s',
            }}
            onMouseEnter={(event) => {
              if (!session.isActive) event.currentTarget.style.background = TINT;
            }}
            onMouseLeave={(event) => {
              if (!session.isActive) event.currentTarget.style.background = 'transparent';
            }}
          >
            <div style={{
              fontFamily: F, fontSize: 12.5,
              color: session.isActive ? ACCENT : TEXT_SOFT,
              fontWeight: session.isActive ? 500 : 400,
              overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
            }}>{session.title}</div>
            {session.time && (
              <div style={{
                fontFamily: FM, fontSize: 9, color: TEXT_FAINT,
                marginTop: 3, letterSpacing: '0.05em',
              }}>{session.time}</div>
            )}
          </button>
        ))}
      </div>
    </aside>
  );
}
