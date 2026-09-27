import { MessageItem } from '../components/chat/MessageItem.tsx';
import { OutputContractRenderer } from '../components/chat/OutputContractRenderer.tsx';
import { SceneIndicator } from '../components/chat/SceneIndicator.tsx';
import { StatusCard } from '../components/chat/StatusCard.tsx';
import { CANVAS, F, FM, RAIL, SURFACE, TEXT_MUTED } from '../components/chat/theme.ts';
import type { Message } from '../lib/types.ts';

/**
 * MOCK RENDER TEST — not engine output.
 *
 * The live engine happens to be emitting plain dialogue almost all the time, so
 * the other three contract channels rarely appear in a smoke run. This screen
 * feeds fixed, hand-written segments through the *real* renderers so their
 * visuals can be inspected without touching the model, the prompt or the
 * engine. Nothing here is ever produced by EROS and nothing here is written
 * back to it.
 *
 * Reachable only when the page is opened with `?phase2_fixture=1`.
 */
export function RenderFixture() {
  const dialogueOnly: Message = {
    id: 'fixture-raw',
    role: 'assistant',
    content: '这一条没有任何 output_contract，正文直接来自 content 字段。',
    timestamp: new Date().toISOString(),
    outputContract: null,
  };

  const fullContract: Message = {
    id: 'fixture-full',
    role: 'assistant',
    content: 'MOCK-ONLY',
    timestamp: new Date().toISOString(),
    outputContract: {
      content: [
        { type: 'dialogue', text: '你来了。', speaker: '裴烬' },
        { type: 'narration', text: '他把烟按灭在杯沿上，没看你。' },
        { type: 'dialogue', text: '站着干什么，坐。', speaker: '裴烬' },
        { type: 'special', text: '窗外的雨声忽然低了下去。', label: '锚' },
      ],
      scene: { time: '23:40', location: '公寓客厅' },
      status_card: { action: '掐灭香烟', status: '疲惫', outfit: '黑色高领', mental: null },
      content_matches_body: true,
      parsed_at: new Date().toISOString(),
    },
  };

  return (
    <div style={{
      minHeight: '100vh', background: CANVAS, fontFamily: F,
      display: 'flex', justifyContent: 'center', padding: '28px 24px',
    }}>
      <div style={{ width: '100%', maxWidth: 720 }}>
        <div style={{
          fontFamily: FM, fontSize: 10, letterSpacing: '0.16em',
          color: TEXT_MUTED, marginBottom: 6,
        }}>MOCK RENDER TEST</div>
        <div style={{
          fontFamily: F, fontSize: 12, color: TEXT_MUTED,
          marginBottom: 18, lineHeight: 1.7,
        }}>
          以下内容全部是手写 fixture，不是 EROS 的真实输出，仅用于检查四种渲染通道的视觉效果。
        </div>

        <div style={{
          background: SURFACE, border: `1px solid ${RAIL}`, borderRadius: 12,
          padding: '16px 18px 6px',
        }}>
          <SceneIndicator time="23:40" location="公寓客厅" />
          <StatusCard card={{ action: '掐灭香烟', status: '疲惫', outfit: '黑色高领', mental: null }} />

          <MessageItem message={fullContract} characterName="裴烬" streaming={false} />
          <MessageItem message={dialogueOnly} characterName="裴烬" streaming={false} />
        </div>

        <div style={{ marginTop: 22 }}>
          <div style={{
            fontFamily: FM, fontSize: 9, letterSpacing: '0.14em',
            color: TEXT_MUTED, marginBottom: 8,
          }}>CHANNEL: special (isolated)</div>
          <div style={{
            background: SURFACE, border: `1px solid ${RAIL}`,
            borderRadius: 12, padding: 16,
          }}>
            <OutputContractRenderer segments={[
              { type: 'special', text: '锚点内容单独渲染。', label: '锚' },
              { type: 'special', text: '没有 label 的 special 块。' },
            ]} />
          </div>
        </div>

        <div style={{ marginTop: 22 }}>
          <div style={{
            fontFamily: FM, fontSize: 9, letterSpacing: '0.14em',
            color: TEXT_MUTED, marginBottom: 8,
          }}>CHANNEL: scene + status absent (must render nothing)</div>
          <div
            data-testid="fixture-empty-channels"
            style={{
              background: SURFACE, border: `1px solid ${RAIL}`,
              borderRadius: 12, padding: 16, minHeight: 40,
            }}
          >
            <SceneIndicator time={null} location={null} />
            <StatusCard card={null} />
            <StatusCard card={{}} />
          </div>
        </div>
      </div>
    </div>
  );
}
