import { F, FM, RAIL, TEXT_MUTED, TINT } from './theme.ts';

/**
 * Light scene strip.
 *
 * Sourced from `output_contract.scene`. When both parts are null the caller
 * passes null and this renders nothing — never a placeholder word like
 * "未知时间".
 */
export function SceneIndicator({ time, location }: { time: string | null; location: string | null }) {
  const parts = [time, location].filter((part): part is string => !!part && !!part.trim());
  if (parts.length === 0) return null;

  return (
    <div
      data-testid="eros-scene"
      style={{
        display: 'flex', alignItems: 'center', gap: 8,
        alignSelf: 'center',
        maxWidth: '100%',
        margin: '2px 0 16px',
        padding: '5px 13px',
        borderRadius: 999,
        background: TINT,
        border: `1px solid ${RAIL}`,
      }}
    >
      <span style={{
        fontFamily: FM, fontSize: 8.5, letterSpacing: '0.16em',
        color: TEXT_MUTED, opacity: 0.8,
      }}>SCENE</span>
      <span style={{
        fontFamily: F, fontSize: 11.5, color: TEXT_MUTED,
        overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
      }}>
        {parts.join(' · ')}
      </span>
    </div>
  );
}
