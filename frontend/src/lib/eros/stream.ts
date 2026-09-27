import type { ErosFrame } from './types.ts';

/**
 * Parse an EROS `text/event-stream` body.
 *
 * The engine writes `Event::default().data(json)` frames and nothing else: no
 * `event:` names, no `id:` fields, one JSON object per `data:` line. This is a
 * line reader, not a generic SSE client — it accepts the engine's shape exactly,
 * and tolerates comments/heartbeats (`:`-prefixed lines) and blank separators.
 */
export async function parseErosStream(
  response: Response,
  onFrame: (frame: ErosFrame) => void,
): Promise<void> {
  if (!response.body) throw new Error('EROS stream had no body');

  const reader = response.body.getReader();
  const decoder = new TextDecoder('utf-8');
  let buffer = '';

  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;

    buffer += decoder.decode(value, { stream: true });
    const lines = buffer.split('\n');
    buffer = lines.pop() ?? '';

    for (const rawLine of lines) {
      const line = rawLine.endsWith('\r') ? rawLine.slice(0, -1) : rawLine;
      if (!line || line.startsWith(':')) continue;
      if (!line.startsWith('data:')) continue;

      const payload = line.slice(5).trim();
      if (!payload || payload === '[DONE]') continue;

      let frame: ErosFrame;
      try {
        frame = JSON.parse(payload) as ErosFrame;
      } catch {
        // A frame the adapter cannot read is dropped rather than fatal: the
        // user-visible contract is that a served reply still renders.
        continue;
      }
      if (!frame || typeof frame.type !== 'string') continue;
      onFrame(frame);
    }
  }

  // Flush a trailing frame that arrived without its newline.
  const tail = buffer.trim();
  if (tail.startsWith('data:')) {
    const payload = tail.slice(5).trim();
    if (payload && payload !== '[DONE]') {
      try {
        const frame = JSON.parse(payload) as ErosFrame;
        if (frame && typeof frame.type === 'string') onFrame(frame);
      } catch {
        /* ignore */
      }
    }
  }
}
