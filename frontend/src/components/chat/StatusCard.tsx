import { F, FM, RAIL, TEXT, TEXT_MUTED } from './theme.ts';
import { statusFieldsFromCard } from '../../lib/eros/phase2View.ts';

/**
 * Light status strip.
 *
 * `status_card` is a free-form engine object whose keys are not contractual.
 * Real keys are read dynamically and only rows with a real value are shown; an
 * empty card renders nothing rather than an empty frame.
 */
export function StatusCard({
  card,
  roleplay = false,
}: {
  card: Record<string, unknown> | null;
  /** 剧情演绎: keep momentary `action` out of the standing strip (P1-4). */
  roleplay?: boolean;
}) {
  const fields = statusFieldsFromCard(card, { roleplay });
  if (fields.length === 0) return null;

  return (
    <div
      data-testid="eros-status-card"
      style={{
        display: 'flex', flexWrap: 'wrap', gap: 5,
        justifyContent: 'center',
        margin: '0 0 16px',
      }}
    >
      {fields.map((field) => (
        <div
          key={field.key}
          style={{
            display: 'flex', alignItems: 'baseline', gap: 6,
            padding: '4px 10px', borderRadius: 8,
            background: '#FFFFFF',
            border: `1px solid ${RAIL}`,
            maxWidth: '100%',
          }}
        >
          <span style={{
            fontFamily: FM, fontSize: 8.5, letterSpacing: '0.12em',
            color: TEXT_MUTED, flexShrink: 0,
          }}>{field.label}</span>
          <span style={{
            fontFamily: F, fontSize: 11.5, color: TEXT,
            overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
          }}>{field.value}</span>
        </div>
      ))}
    </div>
  );
}
