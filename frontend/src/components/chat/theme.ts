/**
 * Shared presentation tokens for the Phase 2 chat shell.
 *
 * These are colours and fonts only — no behaviour, no prompt, no persona. The
 * shell reads everything it displays from EROS; this file just keeps the two
 * dozen inline styles from drifting apart.
 */
export const F = `-apple-system,'PingFang SC','Microsoft YaHei',system-ui,sans-serif`;
export const FM = `'SF Mono','Roboto Mono',ui-monospace,monospace`;

export const ACCENT = '#0071E3';
export const ACCENT_HOVER = '#0077ED';

export const SURFACE = '#FFFFFF';
export const CANVAS = '#FBFBFC';
export const SIDEBAR_BG = '#F7F7F8';
export const RAIL = 'rgba(0,0,0,0.07)';
export const RAIL_STRONG = 'rgba(0,0,0,0.12)';
/** Soft neutral wash for scene/status/narration framing. */
export const TINT = 'rgba(0,0,0,0.035)';

export const TEXT = '#18181B';
export const TEXT_SOFT = '#3F3F46';
export const TEXT_MUTED = '#71717A';
export const TEXT_FAINT = '#A1A1AA';
export const TEXT_GHOST = '#C4C4C8';

/** Reading sizes for the RP stream, kept here so the channels stay in step. */
export const SIZE_DIALOGUE = 14;
export const SIZE_NARRATION = 13.5;
export const LINE_DIALOGUE = 1.95;
export const LINE_NARRATION = 1.9;

export const DANGER = '#FF3B30';
