import { Minus, X } from 'lucide-react';
import type { ReactNode } from 'react';
import { F, RAIL, TEXT, TEXT_FAINT, TEXT_MUTED } from './theme.ts';

/**
 * The desktop build is frameless (`decorations: false` in tauri.conf.json), so
 * this bar doubles as the title bar. In the browser build there is no window to
 * control and the buttons are not rendered.
 */
const IS_TAURI = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

async function runWindowAction(action: 'minimize' | 'close'): Promise<void> {
  // Imported lazily so the browser bundle never pulls the window module in.
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  const current = getCurrentWindow();
  if (action === 'minimize') await current.minimize();
  else await current.close();
}

/**
 * Very light header: the character and an online dot. No model name, no ids, no
 * temperature — none of that belongs in a reader's UI.
 *
 * The bar is the drag region for the frameless window; the two buttons restore
 * the minimize/close affordances the OS would otherwise draw.
 */
export function ChatHeader({ characterName, subtitle }: { characterName: string; subtitle?: string }) {
  return (
    <div
      data-testid="eros-header"
      data-tauri-drag-region
      style={{
        flexShrink: 0,
        display: 'flex', alignItems: 'center', gap: 10,
        height: 52,
        borderBottom: `1px solid ${RAIL}`,
      }}
    >
      <div
        data-tauri-drag-region
        style={{ display: 'flex', alignItems: 'center', gap: 9, minWidth: 0, flex: 1 }}
      >
        <span style={{
          width: 6, height: 6, borderRadius: '50%',
          background: '#34C759', flexShrink: 0,
        }} />
        <span style={{ fontFamily: F, fontSize: 14, color: TEXT, letterSpacing: '-0.01em' }}>
          {characterName}
        </span>
        {subtitle && (
          <span style={{
            fontFamily: F, fontSize: 11.5, color: TEXT_MUTED,
            overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
          }}>{subtitle}</span>
        )}
      </div>

      {IS_TAURI && (
        <div style={{ display: 'flex', alignItems: 'center', gap: 2, flexShrink: 0 }}>
          <WindowButton
            testId="eros-window-minimize"
            title="最小化"
            onClick={() => void runWindowAction('minimize')}
          ><Minus style={{ width: 13, height: 13 }} /></WindowButton>
          <WindowButton
            testId="eros-window-close"
            title="关闭"
            danger
            onClick={() => void runWindowAction('close')}
          ><X style={{ width: 13, height: 13 }} /></WindowButton>
        </div>
      )}
    </div>
  );
}

function WindowButton({ children, onClick, title, testId, danger }: {
  children: ReactNode;
  onClick: () => void;
  title: string;
  testId: string;
  danger?: boolean;
}) {
  return (
    <button
      type="button"
      data-testid={testId}
      title={title}
      aria-label={title}
      onClick={onClick}
      style={{
        width: 28, height: 28, borderRadius: 7,
        border: 'none', background: 'transparent',
        color: danger ? TEXT_FAINT : TEXT_MUTED,
        cursor: 'pointer',
        display: 'flex', alignItems: 'center', justifyContent: 'center',
      }}
      onMouseEnter={(event) => {
        event.currentTarget.style.background = danger ? '#FF3B30' : 'rgba(0,0,0,0.05)';
        event.currentTarget.style.color = danger ? '#FFFFFF' : TEXT;
      }}
      onMouseLeave={(event) => {
        event.currentTarget.style.background = 'transparent';
        event.currentTarget.style.color = danger ? TEXT_FAINT : TEXT_MUTED;
      }}
    >{children}</button>
  );
}
