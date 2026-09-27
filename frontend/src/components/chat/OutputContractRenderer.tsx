import type { ErosContractSegment } from '../../lib/eros/types.ts';
import {
  ACCENT, F, FM, LINE_DIALOGUE, LINE_NARRATION, RAIL, SIZE_DIALOGUE, SIZE_NARRATION,
  TEXT, TEXT_FAINT, TEXT_MUTED, TEXT_SOFT,
} from './theme.ts';

/**
 * Renders `metadata.output_contract.content[]`.
 *
 * The engine already typed every segment; the client only picks a visual
 * channel. Nothing is re-inferred from Markdown, punctuation or keywords, so a
 * segment that says it is `narration` is drawn as narration and a segment with
 * an unknown `type` is drawn as ordinary speech rather than dropped.
 *
 * The caller guarantees at least one non-empty segment (see
 * `contractSegments`); otherwise it falls back to the raw body itself.
 */
export function OutputContractRenderer({ segments }: { segments: ErosContractSegment[] }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
      {segments.map((segment, index) => (
        <Segment key={index} segment={segment} />
      ))}
    </div>
  );
}

function Segment({ segment }: { segment: ErosContractSegment }) {
  if (segment.type === 'narration') {
    return (
      <div
        data-seg="narration"
        style={{
          fontStyle: 'italic',
          color: TEXT_MUTED,
          borderLeft: `2px solid ${RAIL}`,
          paddingLeft: 13,
          fontSize: SIZE_NARRATION,
          lineHeight: LINE_NARRATION,
        }}
      >{segment.text}</div>
    );
  }

  if (segment.type === 'special') {
    return (
      <div
        data-seg="special"
        style={{
          border: `1px solid ${RAIL}`,
          borderLeft: `3px solid ${ACCENT}`,
          borderRadius: 10,
          background: 'rgba(0,113,227,0.045)',
          padding: '10px 13px',
        }}
      >
        {segment.label && (
          <div style={{
            fontFamily: FM, fontSize: 8.5, letterSpacing: '0.16em',
            color: ACCENT, marginBottom: 5, textTransform: 'uppercase',
            opacity: 0.85,
          }}>{segment.label}</div>
        )}
        <div style={{
          fontSize: SIZE_NARRATION, lineHeight: LINE_NARRATION, color: TEXT,
        }}>{segment.text}</div>
      </div>
    );
  }

  // P1-5: `action` is the character's own momentary behaviour — secondary to
  // speech, but not narration. Neither italic nor a rail: it is the character
  // doing something, and the engine already keeps it out of `status_card` in
  // 剧情演绎 so the same line cannot appear twice.
  if (segment.type === 'action') {
    return (
      <div
        data-seg="action"
        style={{
          color: TEXT_SOFT,
          fontSize: SIZE_NARRATION,
          lineHeight: LINE_NARRATION,
        }}
      >{segment.text}</div>
    );
  }

  // `mental` is the weakest channel the engine may emit: an inner state, only
  // when the reply itself gave it a basis. Drawn quieter than narration.
  if (segment.type === 'mental') {
    return (
      <div
        data-seg="mental"
        style={{
          fontStyle: 'italic',
          color: TEXT_FAINT,
          borderLeft: `1px dashed ${RAIL}`,
          paddingLeft: 13,
          fontSize: SIZE_NARRATION,
          lineHeight: LINE_NARRATION,
        }}
      >{segment.text}</div>
    );
  }

  // dialogue — and any type this build does not know about.
  return (
    <div data-seg="dialogue">
      {segment.speaker && (
        <div style={{
          fontFamily: FM, fontSize: 9, letterSpacing: '0.11em',
          color: TEXT_MUTED, marginBottom: 4,
        }}>{segment.speaker}</div>
      )}
      <div style={{ fontSize: SIZE_DIALOGUE, lineHeight: LINE_DIALOGUE, color: TEXT, fontFamily: F }}>
        {segment.text}
      </div>
    </div>
  );
}
