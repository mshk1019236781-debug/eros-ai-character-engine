/**
 * EROS adapter wire types — Phase 1.
 *
 * These mirror the shapes the running engine actually emits; they are copied
 * from source, not guessed:
 *   * SSE frames  -> crates/eros-engine-server/src/pipeline/stream.rs (`ProtocolFrame`)
 *                   serialized by routes/companion_stream.rs as `data:`-only lines.
 *   * history     -> routes/companion.rs `ChatHistoryEntry` (`metadata.output_contract`
 *                   is lifted onto the entry as `output_contract`).
 *   * chat/start  -> routes/companion.rs `StartChatResponse`.
 *   * sessions    -> routes/companion.rs `ListSessionsResponse`.
 *   * character   -> routes/character.rs `CreateCharacterRequest` /
 *                    `CreateCharacterResponse` / `CharacterSummary` /
 *                    `CharacterResponse` (Character Builder V1).
 */

/** `ProtocolFrame` discriminator (`#[serde(tag = "type", rename_all = "snake_case")]`). */
export type ErosFrameType = 'meta' | 'delta' | 'done' | 'final' | 'error' | 'image_request';

export interface ErosMetaFrame {
  type: 'meta';
  message_id: string;
  action_type?: string;
  model?: string;
  continues_from?: string;
}

export interface ErosDeltaFrame {
  type: 'delta';
  message_id: string;
  content: string;
}

export interface ErosDoneFrame {
  type: 'done';
  message_id: string;
  truncated: boolean;
  usage?: Record<string, unknown> | null;
  generation_id?: string | null;
  ghost_fallback?: boolean;
}

export interface ErosFinalFrame {
  type: 'final';
  filtered: boolean;
  prompt_injected?: string[] | null;
  tier?: string | null;
  retries_chat: number;
  retries_filter: number;
  llm_attempts?: unknown[];
  gateway_errors?: unknown[];
}

export interface ErosErrorFrame {
  type: 'error';
  code: string;
  retryable: boolean;
  message: string;
  user_message: string;
  upstream_status?: number | null;
  provider_code?: string | null;
}

export interface ErosImageRequestFrame {
  type: 'image_request';
  message_id: string;
  composed_prompt: string;
  image_ref: unknown;
  aspect_ratio?: string | null;
}

export type ErosFrame =
  | ErosMetaFrame
  | ErosDeltaFrame
  | ErosDoneFrame
  | ErosFinalFrame
  | ErosErrorFrame
  | ErosImageRequestFrame;

/** One typed segment of `output_contract.content[]`. */
export interface ErosContractSegment {
  /**
   * `dialogue` / `narration` / `action` / `mental` / `special` in 剧情演绎, the
   * first three in 微信聊天. Kept open: an unknown type is rendered as ordinary
   * speech rather than dropped.
   */
  type: 'dialogue' | 'narration' | 'action' | 'mental' | 'special' | (string & {});
  text: string;
  speaker?: string | null;
  label?: string | null;
}

export interface ErosOutputContract {
  content?: ErosContractSegment[];
  scene?: { time?: string | null; location?: string | null } | null;
  /**
   * `metadata.output_contract.status_card`.
   *
   * The engine types this as `Option<serde_json::Value>` and writes a free-form
   * object (`{"action":"抬眼"}`, `{"status":"低烧","outfit":"黑色高领"}`), not a
   * string. The renderer therefore reads whatever real keys are present and
   * shows nothing when there are none.
   */
  status_card?: Record<string, unknown> | null;
  content_matches_body?: boolean;
  /**
   * Program-set by the engine (P1-3): which rendering strategy this reply
   * needs. `segments_only` trusts the split, `segments_plus_tail` appends the
   * uncovered `body_tail` after the segments, `body_fallback` drops the split
   * and renders the body. Absent on rows written before the field existed —
   * `renderPlan()` then re-derives the same decision locally.
   */
  render_mode?: 'segments_only' | 'segments_plus_tail' | 'body_fallback' | (string & {}) | null;
  /** Program-set by the engine (P1-3): body text the segments do not cover. */
  body_tail?: string | null;
  parsed_at?: string;
}

/** A `status_card` entry that survived narrowing: a real key with a real value. */
export interface ErosStatusField {
  key: string;
  label: string;
  value: string;
}

/** Flattened scene channel used by the light-weight scene strip. */
export interface ErosScene {
  time: string | null;
  location: string | null;
}

export interface ErosHistoryEntry {
  id: string;
  role: 'user' | 'assistant' | (string & {});
  content: string;
  sent_at?: string;
  created_at?: string;
  extracted_facts?: unknown;
  user_message_id?: string | null;
  output_contract?: ErosOutputContract | null;
  /**
   * Present (always `true`) on the engine-written anchor row of a roleplay
   * opening turn: it drives that one generation and is then out of companion
   * context, so it must never render as something the user said.
   */
  opening?: boolean;
}

export interface ErosHistoryResponse {
  session_id: string;
  messages: ErosHistoryEntry[];
  total: number;
}

export interface ErosStartResponse {
  session_id: string;
  instance_id: string;
  persona_name?: string | null;
  is_new: boolean;
}

export interface ErosSessionListEntry {
  session_id: string;
  instance_id: string;
  is_converted?: boolean;
  last_active_at?: string;
  channel?: string;
}

export interface ErosSessionListResponse {
  user_id: string;
  sessions: ErosSessionListEntry[];
}

// ── Character Builder V1 ───────────────────────────────────

export interface ErosCanonicalExample {
  context: string;
  response: string;
}

export interface ErosRoleplayOptions {
  action: boolean;
  expression: boolean;
  environment: boolean;
  inner_thought: boolean;
  appearance: boolean;
  npc: boolean;
}

/** V1 has exactly two; the engine rejects anything else rather than coercing. */
export type ErosRpMode = 'chat' | 'roleplay';

/**
 * RP Builder V2 — 单角色 / 多人角色.
 *
 * The runtime still binds one session to one `persona_instance`
 * (`engine.chat_sessions.instance_id`), so `multi` is a builder-side
 * configuration today; see the report's Backend Mapping.
 */
export type ErosCharacterMode = 'single' | 'multi';

/** When an Extra Module is allowed to appear in a turn. */
export type ErosExtraModuleTrigger = 'EVERY_TURN' | 'ON_DEMAND';

/**
 * RP Builder V2 — 扩展模块（弹幕 / 任务系统 / 论坛评论 …）.
 *
 * What v2 stores is the module itself — what it is and what it should produce —
 * not a `markdown: true` display flag. How it is rendered is a later,
 * renderer-side decision.
 */
export interface ErosExtraModule {
  id: string;
  name: string;
  instruction: string;
  trigger_mode: ErosExtraModuleTrigger;
}

/** Who may move a System module: only the user, the runtime, or both. */
export type ErosSystemControlMode = 'USER_CONTROLLED' | 'AUTO' | 'HYBRID';

/** RP Builder V2 — System is its own module, not a character. */
export interface ErosSystemConfig {
  name: string;
  persona: string;
  requirements: string[];
  forbidden: string[];
  control_mode: ErosSystemControlMode;
}

/**
 * The RP Builder V2 fields that ride the create payload.
 *
 * `roleplay_requirements` / `user_name` / `user_context` / `extra_modules` are
 * persisted by the engine (RP Config Runtime Mapping V1) into the genome's
 * `art_metadata`, and the Main RP prompt reads them back as
 * `[Roleplay Requirements]` and `[Optional Extra Modules]` — both *after*
 * `[response_contract]`, so an author requirement can never outrank 用户主权.
 *
 * `user_name` / `user_context` are the legacy opening-context keys. The user's
 * own identity is now stored per user (`engine.user_personas`, User Persona V1)
 * and rendered as `[User Identity]`; the genome keys remain readable so a
 * character authored before that layer still renders (§十一).
 *
 * `system_config` and `actor_type` are persisted by the engine
 * (EROS SYSTEM ACTOR V1): `POST /comp/character` stores the module verbatim
 * under `art_metadata.system_config`, and `actor_type: "system"` is what turns
 * the resulting genome into the conversation's System actor — it relaxes the
 * character-sheet requirements and selects the System prompt layer.
 *
 * `character_mode` stays frontend-only: the engine ignores unknown JSON keys
 * rather than rejecting the request.
 */
export interface ErosBuilderPayloadFields {
  /** 要求 — one entry per line. Landed in `art_metadata.roleplay_requirements`. */
  roleplay_requirements?: string[];
  /** 用户 / 当前剧情背景 — opening context, not a user persona system. */
  user_name?: string;
  user_context?: string;
  /** 扩展模块 — rendered as `[Optional Extra Modules]`, emitted via `special`. */
  extra_modules?: ErosExtraModule[];
  /** Persisted to `art_metadata.system_config`. */
  system_config?: ErosSystemConfig | null;
  /** Pending: needs a multi-actor runtime. */
  character_mode?: ErosCharacterMode;
  /**
   * `character` (default) or `system`. Only `system` writes the
   * `art_metadata.actor_type` tag the runtime gates on.
   */
  actor_type?: 'character' | 'system';
}

/** Body of `POST /comp/character`. */
export interface ErosCreateCharacterRequest extends ErosBuilderPayloadFields {
  name: string;
  background_or_profile: string;
  speaking_style: string;
  canonical_examples?: ErosCanonicalExample[];
  forbidden_patterns?: string[];
  forbidden_words?: string[];
  rp_mode?: ErosRpMode;
  roleplay_options?: ErosRoleplayOptions;
  /**
   * An authored `[expression_core]` body — what the Compiler produced, or the
   * user's edit of it. Blank means "fold it out of speaking_style and
   * forbidden_patterns", which is the manual path.
   */
  expression_core?: string;
  /**
   * The raw document the Compiler read. Stored as `source_background` beside
   * the summary, so the sheet survives and can be recompiled later.
   */
  source_text?: string;
}

export interface ErosCreateCharacterResponse {
  genome_id: string;
  name: string;
  rp_mode: ErosRpMode;
  created_at: string;
}

/** One row of `GET /comp/characters`. */
export interface ErosCharacterSummary {
  genome_id: string;
  name: string;
  rp_mode: ErosRpMode;
  created_at: string;
}

/**
 * One row of `GET /comp/character-library` — a character the caller owns.
 *
 * A superset of `ErosCharacterSummary`: it carries the resolved instance and
 * that instance's newest conversation, so the sidebar can switch characters and
 * reopen the right session without creating one just to find out.
 */
export interface ErosCharacterLibraryEntry {
  genome_id: string;
  name: string;
  rp_mode: ErosRpMode;
  created_at: string;
  instance_id: string;
  /** `null` only while this character has no conversation yet. */
  latest_session_id: string | null;
  latest_session_at: string | null;
  conversation_count: number;
}

export interface ErosCharacterLibraryResponse {
  characters: ErosCharacterLibraryEntry[];
}

/**
 * `GET` / `PUT /comp/user-persona` — who the user is, authored once and shared
 * by every character they talk to.
 *
 * Distinct from the character's learned view of the user
 * (`ErosProfile.human_insights`): that one is the character's impression and is
 * per-instance; this one is what the user wrote about themselves. The prompt
 * renders them as two blocks (§九/§十). `null` means "not set", the single
 * representation for both a never-filled and a cleared field.
 */
export interface ErosUserPersona {
  user_name: string | null;
  background: string | null;
  personality: string | null;
  relationship: string | null;
  appearance: string | null;
  notes: string | null;
  /** Absent until the first save. */
  updated_at?: string | null;
}

/** The six editable fields, as the form submits them. */
export type ErosUserPersonaInput = Omit<ErosUserPersona, 'updated_at'>;

/**
 * `GET /comp/character/{genome_id}` — the stored genome exactly as the prompt
 * reads it. `art_metadata` is returned raw: the builder does not re-derive it.
 */
export interface ErosCharacter {
  genome_id: string;
  name: string;
  system_prompt: string;
  art_metadata: Record<string, unknown>;
}

/**
 * `POST /comp/character/compile` — one model call that reads a pasted character
 * sheet and returns a structured draft for the user to confirm.
 *
 * Nothing is persisted by this call. The draft only becomes a character when
 * `createCharacter` is called with what the user confirmed.
 */
export interface ErosCompileCharacterRequest {
  name?: string;
  source_text: string;
  rp_mode?: ErosRpMode;
}

/** `explicit` = the source says it; `inferred` = the compiler concluded it. */
export type ErosFactConfidence = 'explicit' | 'inferred';

export interface ErosCompiledIdentityFact {
  key: string;
  value: string;
  confidence: ErosFactConfidence;
}

export interface ErosCompiledExample {
  context: string;
  response: string;
}

export interface ErosCompiledEvent {
  summary: string;
  /** 1-5; 5 is the most load-bearing for later plot. */
  importance: number;
}

/** The draft itself — exactly the compiler's structured output. */
export interface ErosCompiledCharacter {
  name: string;
  core_identity: string;
  background_summary: string;
  personality: string[];
  identity_facts: ErosCompiledIdentityFact[];
  preferences: { likes: string[]; dislikes: string[] };
  relationship_context: string[];
  speaking_style: string;
  expression_core: string[];
  canonical_examples: ErosCompiledExample[];
  forbidden_patterns: string[];
  important_events: ErosCompiledEvent[];
  uncertain_fields: string[];
}

/** Provenance of one draft. Shown in the builder, never stored on the genome. */
export interface ErosCompileMeta {
  task: string;
  provider: string | null;
  model: string | null;
  latency_ms: number;
  repair_retry: boolean;
  source_chars: number;
  rp_mode: ErosRpMode | null;
}

export interface ErosCompileCharacterResponse {
  draft: ErosCompiledCharacter;
  meta: ErosCompileMeta;
}

/** `GET /comp/instance/{instance_id}/profile` — keys vary by genome. */
export interface ErosProfile {
  instance_id?: string;
  location?: string | null;
  occupation?: string | null;
  current_situation?: string | null;
  updated_at?: string;
  [key: string]: unknown;
}

export type ErosSendStatus = 'ok' | 'aborted' | 'error';

export interface ErosSendError {
  code: string;
  message: string;
  userMessage: string;
  retryable: boolean;
  upstreamStatus?: number | null;
}

export interface ErosSendResult {
  status: ErosSendStatus;
  text: string;
  messageId?: string;
  model?: string;
  actionType?: string;
  generationId?: string | null;
  truncated?: boolean;
  ghostFallback?: boolean;
  usage?: Record<string, unknown> | null;
  final?: ErosFinalFrame;
  error?: ErosSendError;
}
