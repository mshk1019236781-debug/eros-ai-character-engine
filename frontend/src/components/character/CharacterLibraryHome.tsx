import type { ErosCharacterLibraryEntry } from '../../lib/eros/types.ts';
import {
  ACCENT, F, FM, RAIL, SIDEBAR_BG, SURFACE, TEXT, TEXT_FAINT, TEXT_MUTED, TEXT_SOFT, TINT,
} from '../chat/theme.ts';

/**
 * The library as a page of its own.
 *
 * Shown when no character is open — after the open one is deleted (§五), the
 * shell lands here rather than silently opening somebody else's conversation
 * and paying for a session nobody asked for. Nothing on this screen talks to
 * the engine beyond the two lists it is handed: opening a character is one
 * click, and that click is what resumes (or first creates) its session.
 *
 * It is deliberately not a second implementation of the sidebar: the same
 * `ErosCharacterLibraryEntry` rows drive both, and both call the same handlers.
 */
export function CharacterLibraryHome({
  characters,
  archived,
  onOpenCharacter,
  onCreateCharacter,
  onRestoreCharacter,
  busy,
}: {
  characters: ErosCharacterLibraryEntry[];
  archived: ErosCharacterLibraryEntry[];
  onOpenCharacter: (entry: ErosCharacterLibraryEntry) => void;
  onCreateCharacter?: () => void;
  onRestoreCharacter?: (entry: ErosCharacterLibraryEntry) => void | Promise<void>;
  busy?: boolean;
}) {
  return (
    <div style={{
      height: '100vh', display: 'flex', justifyContent: 'center',
      background: SURFACE, overflowY: 'auto',
    }}>
      <div style={{ width: '100%', maxWidth: 560, padding: '68px 28px 56px' }}>
        <div style={{ fontFamily: F, fontSize: 21, color: TEXT, letterSpacing: '-0.01em' }}>角色库</div>
        <div style={{ fontFamily: F, fontSize: 12.5, color: TEXT_MUTED, marginTop: 8, lineHeight: 1.8 }}>
          选择一个角色继续之前的对话，或创建一个新角色。
        </div>

        <div style={{
          marginTop: 26, border: `1px solid ${RAIL}`, borderRadius: 12,
          background: SIDEBAR_BG, overflow: 'hidden',
        }}>
          {characters.length === 0 && (
            <div style={{ padding: '18px 16px', fontFamily: F, fontSize: 12.5, color: TEXT_FAINT }}>
              还没有角色。
            </div>
          )}

          {characters.map((entry, index) => (
            <button
              key={entry.genome_id}
              type="button"
              data-testid="eros-library-open"
              data-genome-id={entry.genome_id}
              onClick={() => onOpenCharacter(entry)}
              disabled={busy}
              style={{
                width: '100%', textAlign: 'left', cursor: busy ? 'default' : 'pointer',
                border: 'none', borderTop: index === 0 ? 'none' : `1px solid ${RAIL}`,
                background: 'transparent', padding: '13px 16px',
                display: 'flex', alignItems: 'center', gap: 12,
              }}
              onMouseEnter={(event) => { if (!busy) event.currentTarget.style.background = TINT; }}
              onMouseLeave={(event) => { event.currentTarget.style.background = 'transparent'; }}
            >
              <div style={{ flex: 1, minWidth: 0 }}>
                <div style={{
                  fontFamily: F, fontSize: 14, color: TEXT,
                  overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
                }}>{entry.name}</div>
                <div style={{
                  fontFamily: FM, fontSize: 9.5, color: TEXT_FAINT, marginTop: 3, letterSpacing: '0.05em',
                }}>
                  {entry.conversation_count} 个对话 · {entry.rp_mode === 'roleplay' ? '剧情演绎' : '聊天'}
                </div>
              </div>
              <span style={{ fontFamily: F, fontSize: 12, color: ACCENT, flexShrink: 0 }}>进入</span>
            </button>
          ))}
        </div>

        {onCreateCharacter && (
          <button
            type="button"
            data-testid="eros-library-create"
            onClick={onCreateCharacter}
            disabled={busy}
            style={{
              marginTop: 14, width: '100%', height: 40, borderRadius: 10,
              border: `1px solid ${RAIL}`, background: '#FFFFFF',
              color: TEXT_SOFT, fontFamily: F, fontSize: 13,
              cursor: busy ? 'default' : 'pointer',
            }}
          >＋ 创建角色</button>
        )}

        {archived.length > 0 && (
          <>
            <div style={{
              marginTop: 34, fontFamily: FM, fontSize: 9, letterSpacing: '0.16em', color: TEXT_FAINT,
            }}>已归档</div>
            <div style={{
              marginTop: 10, border: `1px solid ${RAIL}`, borderRadius: 12,
              background: SIDEBAR_BG, overflow: 'hidden',
            }}>
              {archived.map((entry, index) => (
                <div
                  key={entry.genome_id}
                  data-genome-id={entry.genome_id}
                  style={{
                    padding: '12px 16px', display: 'flex', alignItems: 'center', gap: 12,
                    borderTop: index === 0 ? 'none' : `1px solid ${RAIL}`,
                  }}
                >
                  <div style={{ flex: 1, minWidth: 0 }}>
                    <div style={{ fontFamily: F, fontSize: 13.5, color: TEXT_MUTED }}>{entry.name}</div>
                    <div style={{
                      fontFamily: FM, fontSize: 9.5, color: TEXT_FAINT, marginTop: 3, letterSpacing: '0.05em',
                    }}>{entry.conversation_count} 个对话可恢复</div>
                  </div>
                  {onRestoreCharacter && (
                    <button
                      type="button"
                      data-testid="eros-library-restore"
                      onClick={() => void onRestoreCharacter(entry)}
                      disabled={busy}
                      style={{
                        height: 26, padding: '0 12px', borderRadius: 7,
                        border: `1px solid ${RAIL}`, background: '#FFFFFF',
                        color: TEXT_SOFT, fontFamily: F, fontSize: 12,
                        cursor: busy ? 'default' : 'pointer', flexShrink: 0,
                      }}
                    >恢复</button>
                  )}
                </div>
              ))}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
