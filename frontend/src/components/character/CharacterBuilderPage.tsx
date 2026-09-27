import { useCallback, useEffect, useState } from 'react';
import type { CSSProperties, ReactNode } from 'react';
import {
  MAX_EXTRA_MODULES,
  MULTI_CHARACTER_LIMIT,
  buildCharacterRequestBody,
  buildMultiRpConfig,
  emptyCharacterDraft,
  emptyExtraModule,
  emptySystemDraft,
  extraModulesFromDrafts,
  saveMultiRpConfig,
  splitForbiddenText,
  systemConfigFromDraft,
} from '../../lib/eros/builderDraft.ts';
import type {
  BuilderCharacterDraft,
  BuilderSharedConfig,
  BuilderSystemDraft,
  ExtraModuleDraft,
} from '../../lib/eros/builderDraft.ts';
import {
  compileCharacter,
  createCharacter,
  loadUserPersona,
  saveUserPersona,
} from '../../lib/eros/client.ts';
import { EROS_DEV_USER_ID } from '../../lib/eros/config.ts';
import {
  MULTI_CONVERSATION_STORAGE_KEY,
  SYSTEM_ACTOR_LABEL,
  actorLabelForIndex,
  startMultiChat,
} from '../../lib/eros/multiActor.ts';
import type { MultiActorBindingInput } from '../../lib/eros/multiActor.ts';
import type {
  ErosCharacterMode,
  ErosCompiledCharacter,
  ErosCompileMeta,
  ErosExtraModuleTrigger,
  ErosRoleplayOptions,
  ErosRpMode,
  ErosSystemControlMode,
  ErosUserPersonaInput,
} from '../../lib/eros/types.ts';
import {
  ACCENT, CANVAS, DANGER, F, FM, RAIL, SURFACE, TEXT, TEXT_FAINT, TEXT_MUTED, TEXT_SOFT, TINT,
} from '../chat/theme.ts';

/**
 * Character Builder V2 — the page that turns a filled-in form into an RP.
 *
 * This layer owns no chat state and no prompt text. It collects the fields the
 * engine already accepts (`POST /comp/character`), the ones V2 adds ahead of the
 * engine (see `builderDraft.ts`), and hands the resulting `genome_id` upward;
 * the host opens the existing chat against it. Everything the character will
 * actually be — prompt, expression core, response contract, rules — still lives
 * in the engine, so changing who you talk to never means editing Rust.
 *
 * Two shapes, one submit path. Single mode writes one character and enters the
 * chat. Multi mode configures up to three independent cards and then saves the
 * configuration: the runtime binds a session to a single persona instance
 * today, so the page says so instead of flattening three characters into one
 * prompt and calling that support.
 */

/** The V1 recommendation (spec §十). */
const DEFAULT_ROLEPLAY_OPTIONS: ErosRoleplayOptions = {
  action: true,
  expression: true,
  environment: true,
  inner_thought: false,
  appearance: false,
  npc: true,
};

const ROLEPLAY_OPTION_FIELDS: Array<{ key: keyof ErosRoleplayOptions; label: string }> = [
  { key: 'action', label: '动作描写' },
  { key: 'expression', label: '神态描写' },
  { key: 'environment', label: '环境描写' },
  { key: 'inner_thought', label: '心理描写' },
  { key: 'appearance', label: '外貌 / 衣着' },
  { key: 'npc', label: '允许 NPC' },
];

const TRIGGER_CHOICES: Array<{ value: ErosExtraModuleTrigger; label: string }> = [
  { value: 'EVERY_TURN', label: '每轮可出现' },
  { value: 'ON_DEMAND', label: '仅需要时出现' },
];

const CONTROL_CHOICES: Array<{ value: ErosSystemControlMode; label: string; hint: string }> = [
  { value: 'USER_CONTROLLED', label: '用户控制', hint: '只有用户主动调用时系统才行动。' },
  { value: 'AUTO', label: '自动', hint: 'Runtime 可以在合适的剧情节点主动触发，不是每几轮强行出现。' },
  { value: 'HYBRID', label: '混合', hint: '两者都可以。' },
];

/** How many exemplars the Compiler may hand over. They are never typed by hand. */
const MAX_COMPILED_EXAMPLES = 4;

/**
 * The six fields of 【我的角色 / 用户设定】(§八).
 *
 * The order is the prompt's order (§九): what the form shows top-to-bottom is
 * what `[User Identity]` renders top-to-bottom. Each field is edited on its own
 * because the engine stores them as separate columns — a free-text box would
 * have to be parsed back apart, which is where "who is the user" gets lost.
 */
const USER_PERSONA_FIELDS: Array<{
  key: keyof ErosUserPersonaInput;
  label: string;
  placeholder: string;
  rows: number;
  testId: string;
}> = [
  { key: 'user_name', label: '名字（可选）', placeholder: '例如：林晚', rows: 0, testId: 'eros-user-name' },
  { key: 'background', label: '背景（可选）', placeholder: '例如：外科医生；和角色是大学同学，三年未见。', rows: 3, testId: 'eros-user-background' },
  { key: 'personality', label: '性格（可选）', placeholder: '例如：嘴硬心软，压力大的时候会先沉默。', rows: 3, testId: 'eros-user-personality' },
  { key: 'relationship', label: '与角色的关系（可选）', placeholder: '例如：白远舟的妹妹；和沈砚是前同事。', rows: 2, testId: 'eros-user-relationship' },
  { key: 'appearance', label: '外貌（可选）', placeholder: '例如：短发，左眉有一道旧疤。', rows: 2, testId: 'eros-user-appearance' },
  { key: 'notes', label: '其他备注（可选）', placeholder: '例如：怕黑；不会游泳；讨厌被叫全名。', rows: 2, testId: 'eros-user-notes' },
];

/** The form holds strings; the wire holds `string | null`. Empty means unset. */
type UserPersonaForm = Record<keyof ErosUserPersonaInput, string>;

const EMPTY_USER_PERSONA_FORM: UserPersonaForm = {
  user_name: '',
  background: '',
  personality: '',
  relationship: '',
  appearance: '',
  notes: '',
};

/**
 * The five non-name fields flattened to labelled text.
 *
 * Only the locally saved multi-actor draft reads this; the engine reads the
 * structured row and renders its own `[User Identity]`. Kept so that draft
 * stays readable to a human opening localStorage.
 */
function userContextText(persona: UserPersonaForm): string {
  return USER_PERSONA_FIELDS
    .filter((field) => field.key !== 'user_name')
    .map((field) => {
      const value = persona[field.key].trim();
      return value ? `${field.label.replace('（可选）', '')}：${value}` : '';
    })
    .filter((line) => line.length > 0)
    .join('\n');
}

const REQUIREMENTS_PLACEHOLDER = [
  '- 回复控制在 300～500 字',
  '- 主动推动剧情，不要总等用户先行动',
  '- 冲突不要立刻解决，允许自然加入 NPC',
].join('\n');

const FORBIDDEN_PLACEHOLDER = [
  '不要突然变成过度温柔的人格',
  '不要长篇解释情绪',
  '禁用词：宠溺, 邪魅',
].join('\n');

export interface ActiveCharacter {
  genomeId: string;
  name: string;
}

export function CharacterBuilderPage({
  onCreated,
  onMultiStarted,
}: {
  onCreated: (character: ActiveCharacter) => void;
  /** Opens the multi-actor chat. The conversation id is already stored. */
  onMultiStarted: () => void;
}) {
  const [rpMode, setRpMode] = useState<ErosRpMode>('chat');
  const [characterMode, setCharacterMode] = useState<ErosCharacterMode>('single');
  const [userPersona, setUserPersona] = useState<UserPersonaForm>(EMPTY_USER_PERSONA_FORM);
  const [userOpen, setUserOpen] = useState(false);
  const [options, setOptions] = useState<ErosRoleplayOptions>(DEFAULT_ROLEPLAY_OPTIONS);
  const [modules, setModules] = useState<ExtraModuleDraft[]>([]);
  const [systemOpen, setSystemOpen] = useState(false);
  const [system, setSystem] = useState<BuilderSystemDraft>(emptySystemDraft);
  const [characters, setCharacters] = useState<BuilderCharacterDraft[]>(() => [emptyCharacterDraft()]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');

  const patchUserPersona = useCallback((key: keyof ErosUserPersonaInput, value: string) => {
    setUserPersona((prev) => ({ ...prev, [key]: value }));
  }, []);

  /**
   * §八/§十: the user's identity belongs to the user, not to whichever
   * character happens to be open, so it is read once per Builder visit instead
   * of living inside a per-character draft. A read failure just leaves the form
   * blank — saving still replaces the row wholesale.
   */
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const saved = await loadUserPersona({ userId: EROS_DEV_USER_ID });
        if (cancelled) return;
        setUserPersona({
          user_name: saved.user_name ?? '',
          background: saved.background ?? '',
          personality: saved.personality ?? '',
          relationship: saved.relationship ?? '',
          appearance: saved.appearance ?? '',
          notes: saved.notes ?? '',
        });
      } catch (err: unknown) {
        console.error('[character-builder] loadUserPersona failed', err);
      }
    })();
    return () => { cancelled = true; };
  }, []);

  const patchCharacter = useCallback((id: string, patch: Partial<BuilderCharacterDraft>) => {
    setCharacters((prev) => prev.map((row) => (row.id === id ? { ...row, ...patch } : row)));
  }, []);

  const addCharacter = useCallback(() => {
    setCharacters((prev) => (
      prev.length >= MULTI_CHARACTER_LIMIT ? prev : [...prev, emptyCharacterDraft()]
    ));
  }, []);

  /** Removing is by card id, never by position, so A and C cannot swap text. */
  const removeCharacter = useCallback((id: string) => {
    setCharacters((prev) => (prev.length <= 1 ? prev : prev.filter((row) => row.id !== id)));
  }, []);

  const chooseCharacterMode = useCallback((mode: ErosCharacterMode) => {
    setCharacterMode(mode);
    setError('');
    setNotice('');
    // Single mode creates one character, so leaving multi mode keeps the first
    // card rather than letting hidden ones come back on the next switch.
    if (mode === 'single') setCharacters((prev) => (prev.length > 1 ? [prev[0]] : prev));
  }, []);

  const updateOption = useCallback((key: keyof ErosRoleplayOptions, value: boolean) => {
    setOptions((prev) => ({ ...prev, [key]: value }));
  }, []);

  const patchModule = useCallback((id: string, patch: Partial<ExtraModuleDraft>) => {
    setModules((prev) => prev.map((row) => (row.id === id ? { ...row, ...patch } : row)));
  }, []);

  const addModule = useCallback(() => {
    setModules((prev) => (
      prev.length >= MAX_EXTRA_MODULES ? prev : [...prev, emptyExtraModule()]
    ));
  }, []);

  const removeModule = useCallback((id: string) => {
    setModules((prev) => prev.filter((row) => row.id !== id));
  }, []);

  const patchSystem = useCallback((patch: Partial<BuilderSystemDraft>) => {
    setSystem((prev) => ({ ...prev, ...patch }));
  }, []);

  /** The RP-wide half of the payload, assembled once and reused by both modes. */
  const sharedConfig = useCallback((): BuilderSharedConfig => {
    const roleplay = rpMode === 'roleplay';
    return {
      rpMode,
      characterMode,
      roleplayOptions: options,
      // The genome no longer carries the user's identity — a genome is shared
      // catalogue data, so one user's setup could reach another's prompt. These
      // are only what the local multi draft saves; the engine reads
      // `engine.user_personas` and renders `[User Identity]`.
      userName: userPersona.user_name,
      userContext: userContextText(userPersona),
      extraModules: roleplay ? extraModulesFromDrafts(modules) : [],
      system: roleplay && systemOpen ? systemConfigFromDraft(system) : null,
    };
  }, [rpMode, characterMode, options, userPersona, modules, systemOpen, system]);

  /**
   * One Compiler call for one card, then a merge that cannot overwrite the user.
   *
   * Every merge below only fills a blank: the rule stays `manual user value >
   * compiler value`. `background` is the one exception the brief calls for — the
   * pasted document becomes the summary, and the document itself is kept in
   * `sourceText` so it still reaches `source_background`.
   */
  const analyse = useCallback(async (id: string) => {
    const target = characters.find((row) => row.id === id);
    if (!target || target.compiling) return;
    const raw = target.background.trim();
    if (!raw) {
      patchCharacter(id, { compileError: '请先粘贴人物资料，再让 AI 分析。' });
      return;
    }

    patchCharacter(id, { compiling: true, compileError: '' });
    try {
      const result = await compileCharacter(
        { name: target.name.trim() || undefined, source_text: raw, rp_mode: rpMode },
        { userId: EROS_DEV_USER_ID },
      );
      const { draft } = result;
      setCharacters((prev) => prev.map((row) => {
        if (row.id !== id) return row;
        return {
          ...row,
          compiling: false,
          compileError: '',
          compiled: result,
          sourceText: raw,
          background: summariseDraft(draft) || raw,
          name: row.name.trim() || draft.name,
          speakingStyle: row.speakingStyle.trim() || draft.speaking_style,
          expressionCore: row.expressionCore.trim() || draft.expression_core.join('\n'),
          forbidden: row.forbidden.trim() || draft.forbidden_patterns.join('\n'),
          examples: row.examples.length > 0
            ? row.examples
            : draft.canonical_examples.slice(0, MAX_COMPILED_EXAMPLES),
        };
      }));
    } catch (err: unknown) {
      console.error('[character-builder] compileCharacter failed', err);
      patchCharacter(id, {
        compiling: false,
        compileError: err instanceof Error && err.message
          ? `AI 分析失败：${err.message}`
          : 'AI 分析失败，请重试。',
      });
    }
  }, [characters, patchCharacter, rpMode]);

  const submit = useCallback(async () => {
    if (busy) return;
    setError('');
    setNotice('');

    // §八/§十: save the user's own identity before creating anything, so a
    // character is never created against a persona the engine does not have.
    // This is one DB write — no model call — and a full replace, so clearing a
    // field clears it on the server too.
    try {
      await saveUserPersona(userPersona, { userId: EROS_DEV_USER_ID });
    } catch (err: unknown) {
      console.error('[character-builder] saveUserPersona failed', err);
      return setError('用户设定保存失败，请重试。');
    }

    if (characterMode === 'multi') {
      if (characters.length < 2) return setError('多人模式至少需要 2 个角色。');
      const problem = validateCharacters(characters);
      if (problem) return setError(problem);
      // The engine binds at most three actors, and System is one of them: with
      // a System module configured, two characters is the ceiling. Checked here
      // so the failure is a sentence in the form rather than a 400 after three
      // characters have already been written.
      const shared = sharedConfig();
      if (shared.system && characters.length > 2) {
        return setError('开启 System 时最多 2 个角色（System 占一个 actor 名额）。');
      }
      // The draft is kept, but it is no longer the deliverable: every card
      // becomes its own character, and the engine binds the resulting genomes
      // into one conversation whose actors each own a separate session.
      saveMultiRpConfig(buildMultiRpConfig(sharedConfig(), characters));
      setBusy(true);
      try {
        const actors: MultiActorBindingInput[] = [];
        for (const [index, draft] of characters.entries()) {
          const body = buildCharacterRequestBody(draft, shared);
          // In multi mode System is its own actor, so the module must not also
          // ride every character card — one System, one genome, one session.
          if (shared.system) delete body.system_config;
          const created = await createCharacter(
            body,
            { userId: EROS_DEV_USER_ID },
          );
          actors.push({
            actorLabel: actorLabelForIndex(index),
            genomeId: created.genome_id,
          });
        }
        // §2 / §3: the System is a genome of its own, tagged `system`. The
        // engine gives it the System prompt layer and its own session, and the
        // characters keep exactly the prompts they would have had without it.
        if (shared.system) {
          const systemGenome = await createCharacter(
            {
              name: shared.system.name.trim() || '系统',
              background_or_profile: '',
              speaking_style: '',
              rp_mode: shared.rpMode,
              system_config: shared.system,
              actor_type: 'system',
            },
            { userId: EROS_DEV_USER_ID },
          );
          actors.push({
            actorLabel: SYSTEM_ACTOR_LABEL,
            genomeId: systemGenome.genome_id,
          });
        }
        const conversation = await startMultiChat({ actors });
        window.localStorage.setItem(MULTI_CONVERSATION_STORAGE_KEY, conversation.conversation_id);
        onMultiStarted();
      } catch (err: unknown) {
        console.error('[character-builder] multi create failed', err);
        setError('创建失败，请重试。');
      } finally {
        setBusy(false);
      }
      return;
    }

    const problem = validateCharacters(characters.slice(0, 1));
    if (problem) return setError(problem);

    const draft = characters[0];
    setBusy(true);
    try {
      const created = await createCharacter(
        buildCharacterRequestBody(draft, sharedConfig()),
        { userId: EROS_DEV_USER_ID },
      );
      onCreated({ genomeId: created.genome_id, name: created.name });
    } catch (err: unknown) {
      console.error('[character-builder] createCharacter failed', err);
      setError('创建失败，请重试。');
    } finally {
      setBusy(false);
    }
  }, [busy, characterMode, characters, onCreated, onMultiStarted, sharedConfig, userPersona]);

  return (
    <div
      data-testid="eros-character-builder"
      style={{ height: '100vh', overflowY: 'auto', background: CANVAS, fontFamily: F, color: TEXT }}
    >
      <div style={{ maxWidth: 720, margin: '0 auto', padding: '48px 28px 72px' }}>
        <div style={eyebrowStyle}>RP BUILDER</div>
        <h1 style={titleStyle}>创建 RP</h1>
        <p style={introStyle}>
          先选玩法和角色数量，再把设定填进来。人设、说话方式和禁止事项都会直接进入这个角色的 Prompt。
        </p>

        <Section index={1} title="RP 类型" required hint="只影响 Prompt 的组织方式和可用的高级设置。">
          <div style={choiceRowStyle}>
            <ModeChoice
              testId="eros-rp-mode-chat"
              active={rpMode === 'chat'}
              label="微信聊天"
              hint="以对话为主体，回复直接"
              onClick={() => setRpMode('chat')}
            />
            <ModeChoice
              testId="eros-rp-mode-roleplay"
              active={rpMode === 'roleplay'}
              label="剧情演绎"
              hint="可以写动作、神态、场景，可用扩展模块与系统"
              onClick={() => setRpMode('roleplay')}
            />
          </div>
        </Section>

        <Section index={2} title="角色模式" required>
          <div style={choiceRowStyle}>
            <ModeChoice
              testId="eros-character-mode-single"
              active={characterMode === 'single'}
              label="单角色"
              hint="一个角色，创建后直接进入对话"
              onClick={() => chooseCharacterMode('single')}
            />
            <ModeChoice
              testId="eros-character-mode-multi"
              active={characterMode === 'multi'}
              label="多人角色"
              hint={`最多 ${MULTI_CHARACTER_LIMIT} 个角色，各自独立设定`}
              onClick={() => chooseCharacterMode('multi')}
            />
          </div>
          {characterMode === 'multi' && (
            <div style={hintLineStyle}>
              多人角色目前只完成配置，运行时支持将在下一阶段接入；本轮不会把多个角色拼成一个 Prompt。
            </div>
          )}
        </Section>

        <Section
          index={3}
          title="我的角色 / 用户设定"
          hint="你在故事里的身份。它属于你本人，所有角色共用同一份，创建角色前保存一次。没有特殊设定可以留空。"
          toggle={{
            open: userOpen,
            onToggle: () => setUserOpen((prev) => !prev),
            testId: 'eros-user-context-toggle',
          }}
        >
          {USER_PERSONA_FIELDS.map((field) => (
            <Field key={field.key} label={field.label}>
              {field.rows > 0 ? (
                <TextArea
                  testId={field.testId}
                  value={userPersona[field.key]}
                  rows={field.rows}
                  placeholder={field.placeholder}
                  onChange={(next) => patchUserPersona(field.key, next)}
                />
              ) : (
                <TextInput
                  testId={field.testId}
                  value={userPersona[field.key]}
                  placeholder={field.placeholder}
                  onChange={(next) => patchUserPersona(field.key, next)}
                />
              )}
            </Field>
          ))}
          <div style={hintLineStyle}>
            这一份会以 [User Identity] 注入 Main RP Prompt，与角色身份是两个独立区块：
            它只描述「你是谁」，不会变成角色的人格，也不会替你做决定。
          </div>
        </Section>

        <Section
          index={4}
          title={characterMode === 'single' ? '角色' : `角色（${characters.length}/${MULTI_CHARACTER_LIMIT}）`}
          required
          hint="每个角色一张卡：说话方式、表达核心、要求与禁止事项互相独立。"
        >
          <div style={{ display: 'flex', flexDirection: 'column', gap: 14 }}>
            {characters.map((draft, index) => (
              <CharacterCard
                key={draft.id}
                draft={draft}
                index={index}
                removable={characterMode === 'multi' && characters.length > 1}
                rpMode={rpMode}
                onChange={(patch) => patchCharacter(draft.id, patch)}
                onRemove={() => removeCharacter(draft.id)}
                onAnalyse={() => void analyse(draft.id)}
              />
            ))}
            {characterMode === 'multi' && characters.length < MULTI_CHARACTER_LIMIT && (
              <button type="button" data-testid="eros-character-add" onClick={addCharacter} style={secondaryButtonStyle}>
                ＋ 添加角色
              </button>
            )}
          </div>
        </Section>

        {rpMode === 'roleplay' && (
          <Section index={5} title="剧情演绎设置" hint="只影响这一组描写维度。">
            <div data-testid="eros-roleplay-options" style={optionGridStyle}>
              {ROLEPLAY_OPTION_FIELDS.map((field) => (
                <label key={field.key} style={checkboxRowStyle}>
                  <input
                    type="checkbox"
                    data-testid={`eros-roleplay-option-${field.key}`}
                    checked={options[field.key]}
                    onChange={(event) => updateOption(field.key, event.target.checked)}
                    style={checkboxStyle}
                  />
                  {field.label}
                </label>
              ))}
            </div>
          </Section>
        )}

        {rpMode === 'roleplay' && (
          <Section index={6} title="系统" hint="系统不属于角色，只有需要时才开启。">
            <Toggle
              testId="eros-system-enable"
              checked={systemOpen}
              onChange={setSystemOpen}
              label="添加系统"
              hint="例如任务系统、生存系统这类独立于角色的播报方。"
            />
            {systemOpen && (
              <div data-testid="eros-system-panel" style={cardStyle}>
                <Field label="System 名称（可选）">
                  <TextInput
                    testId="eros-system-name"
                    value={system.name}
                    placeholder="例如：任务系统"
                    onChange={(value) => patchSystem({ name: value })}
                  />
                </Field>
                <Field label="System 设定" required>
                  <TextArea
                    testId="eros-system-persona"
                    value={system.persona}
                    rows={4}
                    placeholder="例如：你是一个冷冰冰的任务系统，只汇报事实，不安慰用户。"
                    onChange={(value) => patchSystem({ persona: value })}
                  />
                </Field>
                <Field label="System 要求" hint="一行一条，可选。">
                  <TextArea
                    testId="eros-system-requirements"
                    value={system.requirements}
                    rows={3}
                    placeholder={'任务必须与当前剧情有关\n不要频繁出现'}
                    onChange={(value) => patchSystem({ requirements: value })}
                  />
                </Field>
                <Field label="System 禁止事项" hint="一行一条，可选。">
                  <TextArea
                    testId="eros-system-forbidden"
                    value={system.forbidden}
                    rows={3}
                    placeholder="不得泄露角色不知道的信息"
                    onChange={(value) => patchSystem({ forbidden: value })}
                  />
                </Field>
                <Field label="控制模式" required hint="前端只保存这个意图，不会自己实现随机触发。">
                  <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
                    {CONTROL_CHOICES.map((choice) => (
                      <RadioRow
                        key={choice.value}
                        testId={`eros-system-control-${choice.value}`}
                        name="eros-system-control"
                        checked={system.control_mode === choice.value}
                        label={choice.label}
                        hint={choice.hint}
                        onSelect={() => patchSystem({ control_mode: choice.value })}
                      />
                    ))}
                  </div>
                </Field>
              </div>
            )}
          </Section>
        )}

        {rpMode === 'roleplay' && (
          <Section
            index={7}
            title={`扩展模块（${modules.length}/${MAX_EXTRA_MODULES}）`}
            hint="例如弹幕、任务系统、论坛评论。模块自带指令，视觉呈现留给之后的前端。"
          >
            <div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
              {modules.map((module, index) => (
                <ExtraModuleCard
                  key={module.id}
                  module={module}
                  index={index}
                  onChange={(patch) => patchModule(module.id, patch)}
                  onRemove={() => removeModule(module.id)}
                />
              ))}
              {modules.length < MAX_EXTRA_MODULES && (
                <button type="button" data-testid="eros-extra-module-add" onClick={addModule} style={secondaryButtonStyle}>
                  ＋ 添加模块
                </button>
              )}
            </div>
          </Section>
        )}

        {error && <div data-testid="eros-builder-error" style={errorBoxStyle}>{error}</div>}
        {!error && notice && <div data-testid="eros-builder-notice" style={noticeBoxStyle}>{notice}</div>}

        <button
          type="button"
          data-testid="eros-character-submit"
          data-submit-mode={characterMode}
          onClick={() => void submit()}
          disabled={busy}
          style={{
            width: '100%', height: 44, borderRadius: 11, border: 'none',
            background: busy ? 'rgba(0,113,227,0.45)' : ACCENT,
            color: '#FFFFFF', fontFamily: F, fontSize: 14, fontWeight: 500,
            cursor: busy ? 'default' : 'pointer',
          }}
        >{busy ? '创建中…' : characterMode === 'multi' ? '创建多人 RP' : '创建并开始 RP'}</button>

        {characterMode === 'multi' && (
          <div style={footerHintStyle}>
            多人模式会为每张卡创建独立角色，并绑定成一个 conversation：每个角色一个 session、一份 Memory。
          </div>
        )}
      </div>
    </div>
  );
}

/**
 * The three fields a character cannot be created without.
 *
 * Checked per card and reported by position, because in multi mode "请填写角色名"
 * is not an actionable message.
 */
function validateCharacters(drafts: BuilderCharacterDraft[]): string {
  for (let index = 0; index < drafts.length; index += 1) {
    const row = drafts[index];
    const where = drafts.length > 1 ? `角色 ${index + 1} 的` : '';
    if (!row.name.trim()) return `请填写${where}角色名。`;
    if (!row.background.trim()) return `请填写${where}人物背景 / 人设。`;
    if (!row.speakingStyle.trim()) return `请填写${where}说话方式。`;
  }
  return '';
}

/**
 * The two brief fields that stand in for the pasted document in the prompt.
 *
 * The document itself goes to `source_background`; only this summary ever
 * reaches `[backstory]`, which is what keeps a 50k-character sheet from becoming
 * a per-turn prompt cost.
 */
function summariseDraft(draft: ErosCompiledCharacter): string {
  return [draft.core_identity.trim(), draft.background_summary.trim()]
    .filter((part) => part.length > 0)
    .join('\n');
}

/**
 * One character card.
 *
 * The card is the unit of isolation in multi mode: every field below reads and
 * writes this card's own draft, addressed by `data-character-id`, so deleting a
 * neighbour can never move text from one character to another.
 */
function CharacterCard({
  draft,
  index,
  removable,
  rpMode,
  onChange,
  onRemove,
  onAnalyse,
}: {
  draft: BuilderCharacterDraft;
  index: number;
  removable: boolean;
  rpMode: ErosRpMode;
  onChange: (patch: Partial<BuilderCharacterDraft>) => void;
  onRemove: () => void;
  onAnalyse: () => void;
}) {
  const split = splitForbiddenText(draft.forbidden);

  return (
    <div
      data-testid="eros-character-card"
      data-character-id={draft.id}
      data-card-index={index}
      style={cardStyle}
    >
      <div style={cardHeaderStyle}>
        <span style={eyebrowStyle}>角色 {index + 1}</span>
        {removable && (
          <button
            type="button"
            data-testid="eros-character-remove"
            data-character-id={draft.id}
            onClick={onRemove}
            style={linkButtonStyle}
          >删除</button>
        )}
      </div>

      <Field label="角色名" required>
        <TextInput
          testId="eros-character-name"
          characterId={draft.id}
          value={draft.name}
          placeholder="例如：裴烬"
          onChange={(value) => onChange({ name: value })}
        />
      </Field>

      <Field
        label="人物背景 / 人设"
        required
        hint="直接填写角色设定，或者粘贴一整段人物资料 / 小说片段，再让 AI 建档。"
      >
        <TextArea
          testId="eros-character-background"
          characterId={draft.id}
          value={draft.background}
          rows={8}
          placeholder="身份、经历、性格、与用户的关系……"
          onChange={(value) => onChange({ background: value })}
        />
        <div style={inlineRowStyle}>
          <button
            type="button"
            data-testid="eros-character-analyse"
            data-character-id={draft.id}
            onClick={onAnalyse}
            disabled={draft.compiling}
            style={{
              ...secondaryButtonStyle,
              borderStyle: 'solid',
              padding: '0 14px',
              color: draft.compiling ? TEXT_FAINT : ACCENT,
              cursor: draft.compiling ? 'default' : 'pointer',
            }}
          >{draft.compiling ? 'AI 分析中…' : 'AI 分析角色'}</button>
          <span style={inlineHintStyle}>
            只调用一次模型。结果填进你还没写的字段，已经填过的不会被覆盖；创建前都可以改。
          </span>
        </div>
        {draft.compileError && (
          <div data-testid="eros-compile-error" style={inlineErrorStyle}>{draft.compileError}</div>
        )}
        {draft.compiled && <CompiledPreview draft={draft.compiled.draft} meta={draft.compiled.meta} />}
      </Field>

      <Field label="说话方式" required hint="这个角色通常如何说话、表达情绪和处理冲突。">
        <TextArea
          testId="eros-character-speaking-style"
          characterId={draft.id}
          value={draft.speakingStyle}
          rows={4}
          placeholder="例如：说话简短，情绪很少直接表达，生气时偏讽刺，不喜欢解释自己。"
          onChange={(value) => onChange({ speakingStyle: value })}
        />
      </Field>

      <Field label="表达核心" hint="这个角色稳定的表达与反应方式。留空时由说话方式和禁止事项自动生成。">
        <TextArea
          testId="eros-character-expression-core"
          characterId={draft.id}
          value={draft.expressionCore}
          rows={4}
          placeholder="例如：情绪外显，先给判断再给原因；被关心时用讽刺挡回去，而不是道谢。"
          onChange={(value) => onChange({ expressionCore: value })}
        />
      </Field>

      <Field label="要求" hint="希望模型必须怎么演、怎么输出、怎么推进。一行一条，可选。">
        <TextArea
          testId="eros-character-requirements"
          characterId={draft.id}
          value={draft.requirements}
          rows={4}
          placeholder={REQUIREMENTS_PLACEHOLDER}
          onChange={(value) => onChange({ requirements: value })}
        />
      </Field>

      <Field
        label="禁止事项"
        hint="一行一条。行为 / 人格 / 表达禁止直接写；只作为词过滤的写「禁用词：宠溺, 邪魅」。"
      >
        <TextArea
          testId="eros-character-forbidden"
          characterId={draft.id}
          value={draft.forbidden}
          rows={4}
          placeholder={FORBIDDEN_PLACEHOLDER}
          onChange={(value) => onChange({ forbidden: value })}
        />
        <div data-testid="eros-forbidden-split" data-character-id={draft.id} style={inlineHintStyle}>
          提交时拆成 {split.patterns.length} 条禁止规则 · {split.words.length} 个禁用词
        </div>
      </Field>

      {rpMode === 'roleplay' && draft.examples.length > 0 && (
        <div style={inlineHintStyle}>
          这次 AI 建档还带回了 {draft.examples.length} 条说话示例，会一起保存，不需要你手写。
        </div>
      )}
    </div>
  );
}

/** One Extra Module, in the form: what it is, what it produces, when it may run. */
function ExtraModuleCard({
  module,
  index,
  onChange,
  onRemove,
}: {
  module: ExtraModuleDraft;
  index: number;
  onChange: (patch: Partial<ExtraModuleDraft>) => void;
  onRemove: () => void;
}) {
  return (
    <div data-testid="eros-extra-module" data-module-id={module.id} data-module-index={index} style={cardStyle}>
      <div style={cardHeaderStyle}>
        <span style={eyebrowStyle}>模块 {index + 1}</span>
        <button
          type="button"
          data-testid="eros-extra-module-remove"
          data-module-id={module.id}
          onClick={onRemove}
          style={linkButtonStyle}
        >删除</button>
      </div>

      <Field label="名称">
        <TextInput
          testId="eros-extra-module-name"
          moduleId={module.id}
          value={module.name}
          placeholder="弹幕 / 任务系统 / 论坛评论 / 直播评论"
          onChange={(value) => onChange({ name: value })}
        />
      </Field>

      <Field label="生成指令">
        <TextArea
          testId="eros-extra-module-instruction"
          moduleId={module.id}
          value={module.instruction}
          rows={3}
          placeholder="例如：以围观读者视角评论当前剧情，每次 3～5 条。"
          onChange={(value) => onChange({ instruction: value })}
        />
      </Field>

      <Field label="触发方式">
        <div style={{ display: 'flex', flexWrap: 'wrap', gap: 8 }}>
          {TRIGGER_CHOICES.map((choice) => (
            <button
              key={choice.value}
              type="button"
              data-testid={`eros-extra-module-trigger-${choice.value}`}
              data-module-id={module.id}
              data-selected={module.trigger_mode === choice.value ? 'true' : 'false'}
              onClick={() => onChange({ trigger_mode: choice.value })}
              style={pillStyle(module.trigger_mode === choice.value)}
            >{choice.label}</button>
          ))}
        </div>
      </Field>
    </div>
  );
}

/**
 * What the Compiler found, shown for confirmation rather than applied silently.
 *
 * Explicit facts and inferred ones are labelled apart on purpose: the point of
 * the split is that the user can see which lines the source supports and which
 * the model concluded, and drop the latter.
 */
function CompiledPreview({ draft, meta }: { draft: ErosCompiledCharacter; meta: ErosCompileMeta }) {
  const empty = '（无）';
  return (
    <div
      data-testid="eros-compiled-preview"
      style={{
        marginTop: 12, border: `1px solid ${RAIL}`, borderRadius: 10,
        background: TINT, padding: '12px 14px', fontSize: 12.5, lineHeight: 1.8,
      }}
    >
      <div style={{ display: 'flex', justifyContent: 'space-between', gap: 10, marginBottom: 8, flexWrap: 'wrap' }}>
        <span style={eyebrowStyle}>AI 建档结果</span>
        <span style={{ fontSize: 11, color: TEXT_MUTED }}>
          {meta.model ?? meta.task} · {(meta.latency_ms / 1000).toFixed(1)}s
          {meta.repair_retry ? ' · 已重试一次' : ''} · 原文 {meta.source_chars} 字
        </span>
      </div>

      <PreviewRow label="核心身份" value={draft.core_identity || empty} />
      <PreviewRow label="背景摘要" value={draft.background_summary || empty} />
      <PreviewRow label="性格" value={draft.personality.join('、') || empty} />
      <PreviewRow label="说话方式" value={draft.speaking_style || empty} />
      <PreviewRow label="表达核心" value={draft.expression_core.join('\n') || empty} />
      <PreviewRow label="重要关系" value={draft.relationship_context.join('\n') || empty} />

      <div data-testid="eros-compiled-facts">
        <span style={{ color: TEXT_MUTED }}>身份事实</span>
        {draft.identity_facts.length === 0 ? (
          <div style={{ color: TEXT_MUTED }}>{empty}</div>
        ) : (
          draft.identity_facts.map((fact, index) => (
            <div key={`${fact.key}-${index}`}>
              {fact.key}：{fact.value}
              <span style={{ color: TEXT_FAINT }}>
                （{fact.confidence === 'explicit' ? '原文明确' : '推断'}）
              </span>
            </div>
          ))
        )}
      </div>

      <PreviewRow
        label="重要事件"
        value={draft.important_events.map((event) => `${event.summary}（${event.importance}）`).join('\n') || empty}
      />
      <PreviewRow label="不确定内容" value={draft.uncertain_fields.join('\n') || empty} />
    </div>
  );
}

function PreviewRow({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <span style={{ color: TEXT_MUTED }}>{label}</span>
      <div style={{ whiteSpace: 'pre-wrap' }}>{value}</div>
    </div>
  );
}

/**
 * A numbered block of the form.
 *
 * `toggle` is what keeps the optional half out of the way: without it the block
 * is always open, with it the whole section collapses to its title.
 */
function Section({
  index,
  title,
  hint,
  required,
  children,
  toggle,
}: {
  index: number;
  title: string;
  hint?: string;
  required?: boolean;
  children: ReactNode;
  toggle?: { open: boolean; onToggle: () => void; testId?: string };
}) {
  const header = (
    <div style={sectionHeaderStyle}>
      <span style={sectionIndexStyle}>{index}</span>
      <span style={{ fontSize: 13.5, fontWeight: 500 }}>{title}</span>
      {required && <span style={{ fontSize: 11, color: DANGER }}>*</span>}
      {toggle && (
        <span style={{ marginLeft: 'auto', fontSize: 11.5, color: TEXT_MUTED }}>
          {toggle.open ? '收起' : '展开'}
        </span>
      )}
    </div>
  );

  const open = !toggle || toggle.open;
  return (
    <section style={{ marginBottom: 24 }}>
      {toggle ? (
        <button type="button" data-testid={toggle.testId} onClick={toggle.onToggle} style={sectionToggleStyle}>
          {header}
        </button>
      ) : header}
      {open && (
        <>
          {hint && <div style={sectionHintStyle}>{hint}</div>}
          {children}
        </>
      )}
    </section>
  );
}

/** One labelled field inside a card. */
function Field({
  label,
  hint,
  required,
  children,
}: {
  label: string;
  hint?: string;
  required?: boolean;
  children: ReactNode;
}) {
  return (
    <div style={{ marginBottom: 14 }}>
      <div style={fieldLabelStyle}>
        {label}
        {required && <span style={{ color: DANGER, marginLeft: 6 }}>*</span>}
      </div>
      {hint && <div style={fieldHintStyle}>{hint}</div>}
      {children}
    </div>
  );
}

/** The two-or-three-way selector used by RP 类型 and 角色模式. */
function ModeChoice({
  active,
  label,
  hint,
  onClick,
  testId,
}: {
  active: boolean;
  label: string;
  hint: string;
  onClick: () => void;
  testId: string;
}) {
  return (
    <button
      type="button"
      data-testid={testId}
      data-active={active ? 'true' : 'false'}
      onClick={onClick}
      style={{
        flex: '1 1 200px', textAlign: 'left',
        border: `1px solid ${active ? ACCENT : RAIL}`,
        background: active ? 'rgba(0,113,227,0.05)' : SURFACE,
        borderRadius: 10, padding: '11px 13px', cursor: 'pointer', fontFamily: F,
      }}
    >
      <div style={{ fontSize: 13, color: active ? ACCENT : TEXT, fontWeight: 500 }}>{label}</div>
      <div style={{ fontSize: 11.5, color: TEXT_MUTED, marginTop: 3 }}>{hint}</div>
    </button>
  );
}

function Toggle({
  testId,
  checked,
  onChange,
  label,
  hint,
}: {
  testId: string;
  checked: boolean;
  onChange: (value: boolean) => void;
  label: string;
  hint?: string;
}) {
  return (
    <label style={{ ...radioRowStyle, marginBottom: 12 }}>
      <input
        type="checkbox"
        data-testid={testId}
        checked={checked}
        onChange={(event) => onChange(event.target.checked)}
        style={{ ...checkboxStyle, marginTop: 2 }}
      />
      <span>
        <span style={{ fontSize: 13, color: TEXT }}>{label}</span>
        {hint && <div style={fieldHintStyle}>{hint}</div>}
      </span>
    </label>
  );
}

function RadioRow({
  testId,
  name,
  checked,
  label,
  hint,
  onSelect,
}: {
  testId: string;
  name: string;
  checked: boolean;
  label: string;
  hint: string;
  onSelect: () => void;
}) {
  return (
    <label style={radioRowStyle}>
      <input
        type="radio"
        name={name}
        data-testid={testId}
        checked={checked}
        onChange={onSelect}
        style={{ ...checkboxStyle, marginTop: 2 }}
      />
      <span>
        <span style={{ fontSize: 12.5, color: TEXT }}>{label}</span>
        <div style={fieldHintStyle}>{hint}</div>
      </span>
    </label>
  );
}

function TextInput({
  value,
  placeholder,
  onChange,
  testId,
  characterId,
  moduleId,
}: {
  value: string;
  placeholder: string;
  onChange: (value: string) => void;
  testId?: string;
  characterId?: string;
  moduleId?: string;
}) {
  return (
    <input
      type="text"
      value={value}
      placeholder={placeholder}
      data-testid={testId}
      data-character-id={characterId}
      data-module-id={moduleId}
      onChange={(event) => onChange(event.target.value)}
      style={inputStyle}
    />
  );
}

function TextArea({
  value,
  placeholder,
  onChange,
  rows,
  testId,
  characterId,
  moduleId,
}: {
  value: string;
  placeholder: string;
  onChange: (value: string) => void;
  rows: number;
  testId?: string;
  characterId?: string;
  moduleId?: string;
}) {
  return (
    <textarea
      value={value}
      placeholder={placeholder}
      data-testid={testId}
      data-character-id={characterId}
      data-module-id={moduleId}
      rows={rows}
      onChange={(event) => onChange(event.target.value)}
      style={{ ...inputStyle, lineHeight: 1.75, resize: 'vertical' }}
    />
  );
}

const eyebrowStyle: CSSProperties = {
  fontFamily: FM, fontSize: 9.5, letterSpacing: '0.16em', color: TEXT_FAINT,
};

const titleStyle: CSSProperties = {
  fontSize: 24, fontWeight: 600, letterSpacing: '-0.02em', margin: '10px 0 6px',
};

const introStyle: CSSProperties = {
  fontSize: 13, color: TEXT_MUTED, lineHeight: 1.8, margin: '0 0 28px',
};

const choiceRowStyle: CSSProperties = { display: 'flex', gap: 10, flexWrap: 'wrap' };

const hintLineStyle: CSSProperties = {
  marginTop: 10, fontSize: 11.5, color: TEXT_MUTED, lineHeight: 1.7,
};

const cardStyle: CSSProperties = {
  border: `1px solid ${RAIL}`, borderRadius: 12, background: SURFACE, padding: '16px 16px 6px',
};

const cardHeaderStyle: CSSProperties = {
  display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 12,
};

const fieldLabelStyle: CSSProperties = {
  fontSize: 12.5, fontWeight: 500, color: TEXT, marginBottom: 5,
};

const fieldHintStyle: CSSProperties = {
  fontSize: 11.5, color: TEXT_MUTED, lineHeight: 1.7, marginTop: 2,
};

const inlineRowStyle: CSSProperties = {
  display: 'flex', alignItems: 'center', gap: 10, marginTop: 10, flexWrap: 'wrap',
};

const inlineHintStyle: CSSProperties = {
  fontSize: 11.5, color: TEXT_MUTED, lineHeight: 1.7, marginTop: 8,
};

const inlineErrorStyle: CSSProperties = { marginTop: 8, fontSize: 12, color: DANGER };

const sectionHeaderStyle: CSSProperties = {
  display: 'flex', alignItems: 'baseline', gap: 8, marginBottom: 8,
};

const sectionIndexStyle: CSSProperties = { fontFamily: FM, fontSize: 10, color: TEXT_FAINT };

const sectionToggleStyle: CSSProperties = {
  display: 'block', width: '100%', textAlign: 'left', border: 'none',
  background: 'transparent', padding: 0, cursor: 'pointer', fontFamily: F, color: TEXT,
};

const sectionHintStyle: CSSProperties = {
  fontSize: 11.5, color: TEXT_MUTED, marginBottom: 8, lineHeight: 1.7,
};

const optionGridStyle: CSSProperties = {
  display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(160px, 1fr))', gap: 10,
};

const checkboxRowStyle: CSSProperties = {
  display: 'flex', alignItems: 'center', gap: 8, fontSize: 12.5, color: TEXT_SOFT, cursor: 'pointer',
};

const checkboxStyle: CSSProperties = { width: 15, height: 15, accentColor: ACCENT };

const radioRowStyle: CSSProperties = { display: 'flex', alignItems: 'flex-start', gap: 9, cursor: 'pointer' };

const errorBoxStyle: CSSProperties = {
  marginBottom: 14, fontSize: 12, color: DANGER,
  background: 'rgba(255,59,48,0.05)', border: '1px solid rgba(255,59,48,0.15)',
  borderRadius: 9, padding: '9px 12px',
};

const noticeBoxStyle: CSSProperties = {
  marginBottom: 14, fontSize: 12, color: TEXT_SOFT,
  background: TINT, border: `1px solid ${RAIL}`, borderRadius: 9, padding: '9px 12px', lineHeight: 1.7,
};

const footerHintStyle: CSSProperties = {
  marginTop: 10, fontSize: 11.5, color: TEXT_MUTED, lineHeight: 1.7,
};

const inputStyle: CSSProperties = {
  width: '100%',
  boxSizing: 'border-box',
  border: `1px solid ${RAIL}`,
  borderRadius: 10,
  background: SURFACE,
  padding: '10px 12px',
  fontFamily: F,
  fontSize: 13,
  color: TEXT,
  outline: 'none',
};

const secondaryButtonStyle: CSSProperties = {
  height: 34, borderRadius: 9, border: '1px dashed rgba(0,0,0,0.16)',
  background: 'transparent', color: TEXT_MUTED, fontFamily: F, fontSize: 12.5, cursor: 'pointer',
};

const linkButtonStyle: CSSProperties = {
  border: 'none', background: 'transparent', color: TEXT_MUTED,
  fontFamily: F, fontSize: 11.5, cursor: 'pointer', padding: 0,
};

/** Selected / unselected pill for the binary Extra Module trigger. */
function pillStyle(active: boolean): CSSProperties {
  return {
    height: 32, padding: '0 14px', borderRadius: 999,
    border: `1px solid ${active ? ACCENT : RAIL}`,
    background: active ? 'rgba(0,113,227,0.06)' : SURFACE,
    color: active ? ACCENT : TEXT_MUTED,
    fontFamily: F, fontSize: 12.5, cursor: 'pointer',
  };
}
