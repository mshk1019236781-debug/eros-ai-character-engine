/**
 * RP Builder V2 — the draft behind the create page.
 *
 * No React and no rendering live here: this file owns the shape of a
 * half-finished RP configuration, the deterministic rules the form applies on
 * submit, and the `POST /comp/character` body those rules produce. Splitting it
 * out of the component is what keeps the payload mapping readable — the merge
 * and split rules are the part of the builder that has to stay honest.
 */
import type {
  ErosCanonicalExample,
  ErosCharacterMode,
  ErosCompiledCharacter,
  ErosCompileMeta,
  ErosCreateCharacterRequest,
  ErosExtraModule,
  ErosRoleplayOptions,
  ErosRpMode,
  ErosSystemConfig,
  ErosSystemControlMode,
} from './types.ts';

/** The three limits the brief fixes for V2. */
export const MULTI_CHARACTER_LIMIT = 3;
export const MAX_EXTRA_MODULES = 3;
/** Multi-character configurations are parked here until the runtime can use them. */
export const MULTI_CONFIG_STORAGE_KEY = 'eros_multi_rp_config_draft';

/**
 * One character card.
 *
 * Every card owns its own copy of every field — `speakingStyle`, `expressionCore`,
 * `requirements` and `forbidden` are never shared between cards, which is what
 * makes add/remove safe in multi mode.
 */
export interface BuilderCharacterDraft {
  /** Stable across renames and deletions; the React key and the smoke handles. */
  id: string;
  name: string;
  background: string;
  speakingStyle: string;
  expressionCore: string;
  /** 要求 — how the model should act / output / advance the plot. */
  requirements: string;
  /** The one 禁止事项 box: behaviour, persona, expression and literal words. */
  forbidden: string;
  /** The document the Compiler read; kept so it can still reach `source_background`. */
  sourceText: string;
  /**
   * Compiler output only. V2 stopped asking the user to write exemplars by hand,
   * but the Compiler may still find some in a pasted sheet, and the field keeps
   * them (and the wire shape) intact.
   */
  examples: ErosCanonicalExample[];
  compiled: { draft: ErosCompiledCharacter; meta: ErosCompileMeta } | null;
  compiling: boolean;
  compileError: string;
}

/** An Extra Module as the form edits it (the wire shape plus nothing). */
export type ExtraModuleDraft = ErosExtraModule;

/** The System module as the form edits it: four free-text boxes and a mode. */
export interface BuilderSystemDraft {
  name: string;
  persona: string;
  requirements: string;
  forbidden: string;
  control_mode: ErosSystemControlMode;
}

/** Everything that belongs to the RP rather than to one character. */
export interface BuilderSharedConfig {
  rpMode: ErosRpMode;
  characterMode: ErosCharacterMode;
  roleplayOptions: ErosRoleplayOptions;
  userName: string;
  userContext: string;
  extraModules: ErosExtraModule[];
  /** `null` when System is off, unnamed, or the mode cannot carry one. */
  system: ErosSystemConfig | null;
}

let idSeq = 0;

/** Readable, unique, and stable for the lifetime of the page. */
function nextId(prefix: string): string {
  idSeq += 1;
  return `${prefix}-${Date.now().toString(36)}-${idSeq}-${Math.random().toString(36).slice(2, 8)}`;
}

export function emptyCharacterDraft(): BuilderCharacterDraft {
  return {
    id: nextId('char'),
    name: '',
    background: '',
    speakingStyle: '',
    expressionCore: '',
    requirements: '',
    forbidden: '',
    sourceText: '',
    examples: [],
    compiled: null,
    compiling: false,
    compileError: '',
  };
}

export function emptyExtraModule(): ExtraModuleDraft {
  return { id: nextId('module'), name: '', instruction: '', trigger_mode: 'EVERY_TURN' };
}

/**
 * Conservative on purpose: a System that only moves when the user calls it is
 * the state that cannot surprise anybody. `AUTO` is an explicit choice.
 */
export function emptySystemDraft(): BuilderSystemDraft {
  return { name: '', persona: '', requirements: '', forbidden: '', control_mode: 'USER_CONTROLLED' };
}

/** One entry per line, blanks dropped. */
export function linesFrom(text: string): string[] {
  return text
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
}

/** Comma / 、 / whitespace separated tokens, blanks dropped. */
export function wordsFrom(text: string): string[] {
  return text
    .split(/[\s,，、]+/)
    .map((word) => word.trim())
    .filter((word) => word.length > 0);
}

function unique(list: string[]): string[] {
  return [...new Set(list)];
}

const WORD_PREFIX = /^(?:禁用词|禁用词语|词|词汇)\s*[:：]\s*(.*)$/;
const PATTERN_PREFIX = /^(?:禁止事项|禁止|规则)\s*[:：]\s*(.*)$/;

/** A bare line of two or more short comma-separated tokens reads as a word list. */
function looksLikeWordList(line: string): boolean {
  const tokens = line.split(/[,，、]/).map((token) => token.trim()).filter((token) => token.length > 0);
  if (tokens.length < 2) return false;
  return tokens.every((token) => token.length <= 6 && !/\s/.test(token));
}

/**
 * Split the single 禁止事项 box into the two lists the engine still keeps.
 *
 * The user is not asked to learn that `forbidden_patterns` and `forbidden_words`
 * are different columns, so the split is a frontend rule and it is deliberately
 * boring and visible in the form:
 *
 *   * `禁用词：人家, 小可爱`            -> words
 *   * `禁止：不要突然变得过度温柔`      -> patterns
 *   * `人家, 小可爱` (all-short tokens) -> words
 *   * anything else                     -> patterns
 *
 * A short line with no separator ("不要说教") stays a pattern: treating it as a
 * literal filter token would block text the user never meant to ban.
 */
export function splitForbiddenText(text: string): { patterns: string[]; words: string[] } {
  const patterns: string[] = [];
  const words: string[] = [];

  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line) continue;

    const wordMatch = line.match(WORD_PREFIX);
    if (wordMatch) {
      words.push(...wordsFrom(wordMatch[1]));
      continue;
    }

    const patternMatch = line.match(PATTERN_PREFIX);
    if (patternMatch) {
      const rest = patternMatch[1].trim();
      if (rest) patterns.push(rest);
      continue;
    }

    if (looksLikeWordList(line)) words.push(...wordsFrom(line));
    else patterns.push(line);
  }

  return { patterns: unique(patterns), words: unique(words) };
}

/** Drop modules the user opened but never filled in. */
export function extraModulesFromDrafts(drafts: ExtraModuleDraft[]): ErosExtraModule[] {
  return drafts
    .map((draft) => ({
      id: draft.id,
      name: draft.name.trim(),
      instruction: draft.instruction.trim(),
      trigger_mode: draft.trigger_mode,
    }))
    .filter((module) => module.name.length > 0 || module.instruction.length > 0);
}

/** `null` unless the user actually wrote a System persona. */
export function systemConfigFromDraft(draft: BuilderSystemDraft): ErosSystemConfig | null {
  const persona = draft.persona.trim();
  if (!persona) return null;
  return {
    name: draft.name.trim(),
    persona,
    requirements: linesFrom(draft.requirements),
    forbidden: linesFrom(draft.forbidden),
    control_mode: draft.control_mode,
  };
}

/**
 * The `POST /comp/character` body for one character card.
 *
 * Optional keys are omitted rather than sent empty, which is what V1 did: the
 * engine treats "absent" and "blank" differently for several of these fields
 * (a blank `expression_core` means "fold one out of the style", not "no core").
 */
export function buildCharacterRequestBody(
  draft: BuilderCharacterDraft,
  shared: BuilderSharedConfig,
): ErosCreateCharacterRequest {
  const { patterns, words } = splitForbiddenText(draft.forbidden);
  const requirements = linesFrom(draft.requirements);
  const examples = draft.examples
    .map((example) => ({ context: example.context.trim(), response: example.response.trim() }))
    .filter((example) => example.context.length > 0 && example.response.length > 0);

  const body: ErosCreateCharacterRequest = {
    name: draft.name.trim(),
    background_or_profile: draft.background.trim(),
    speaking_style: draft.speakingStyle.trim(),
    rp_mode: shared.rpMode,
    roleplay_options: shared.roleplayOptions,
    character_mode: shared.characterMode,
  };

  if (examples.length > 0) body.canonical_examples = examples;
  if (patterns.length > 0) body.forbidden_patterns = patterns;
  if (words.length > 0) body.forbidden_words = words;

  const expressionCore = draft.expressionCore.trim();
  if (expressionCore) body.expression_core = expressionCore;

  const sourceText = draft.sourceText.trim();
  if (sourceText) body.source_text = sourceText;

  // ── authored steering, persisted by the engine ──────────────
  // RP Config Runtime Mapping V1: the engine stores these in the genome's
  // `art_metadata` and the Main RP prompt reads them back — the requirements
  // after `[response_contract]`, the modules as `[Optional Extra Modules]`.
  if (requirements.length > 0) body.roleplay_requirements = requirements;

  // The user's own identity is deliberately NOT written into the genome
  // (User Persona V1, §十). A genome is shared catalogue data, so one user's
  // name and background stored there would reach every other user's prompt.
  // It is saved per user through `PUT /comp/user-persona` and rendered as
  // `[User Identity]`; the legacy genome keys stay readable as the fallback for
  // characters authored before that layer existed (§十一).

  if (shared.rpMode === 'roleplay' && shared.extraModules.length > 0) {
    body.extra_modules = shared.extraModules;
  }
  if (shared.rpMode === 'roleplay' && shared.system) {
    body.system_config = shared.system;
  }

  return body;
}

/**
 * The multi-character configuration, saved rather than sent.
 *
 * The shape is the single-character payload repeated once per card, so nothing
 * has to be re-derived when the runtime grows multi-actor sessions.
 */
export interface MultiRpConfigDraft {
  saved_at: string;
  rp_mode: ErosRpMode;
  character_mode: 'multi';
  user_name: string;
  user_context: string;
  roleplay_options: ErosRoleplayOptions;
  extra_modules: ErosExtraModule[];
  system_config: ErosSystemConfig | null;
  characters: Array<{
    id: string;
    name: string;
    background_or_profile: string;
    speaking_style: string;
    expression_core?: string;
    character_rules: string[];
    forbidden_patterns: string[];
    forbidden_words: string[];
  }>;
}

export function buildMultiRpConfig(
  shared: BuilderSharedConfig,
  characters: BuilderCharacterDraft[],
): MultiRpConfigDraft {
  const roleplay = shared.rpMode === 'roleplay';
  return {
    saved_at: new Date().toISOString(),
    rp_mode: shared.rpMode,
    character_mode: 'multi',
    user_name: shared.userName.trim(),
    user_context: shared.userContext.trim(),
    roleplay_options: shared.roleplayOptions,
    extra_modules: roleplay ? shared.extraModules : [],
    system_config: roleplay ? shared.system : null,
    characters: characters.map((draft) => {
      const { patterns, words } = splitForbiddenText(draft.forbidden);
      const expressionCore = draft.expressionCore.trim();
      return {
        id: draft.id,
        name: draft.name.trim(),
        background_or_profile: draft.background.trim(),
        speaking_style: draft.speakingStyle.trim(),
        ...(expressionCore ? { expression_core: expressionCore } : {}),
        character_rules: linesFrom(draft.requirements),
        forbidden_patterns: patterns,
        forbidden_words: words,
      };
    }),
  };
}

/** Best effort: storage being unavailable must never fail the builder. */
export function saveMultiRpConfig(config: MultiRpConfigDraft): boolean {
  try {
    window.localStorage.setItem(MULTI_CONFIG_STORAGE_KEY, JSON.stringify(config));
    return true;
  } catch {
    return false;
  }
}
