import type { ErosOutputContract } from './eros/types.ts';

export interface SoulProfile {
  coreTraits: Array<{ name: string; description: string; example: string }>;
  languageStyle: {
    vocabulary: string[];
    patterns: string[];
    rhetoric: string;
    tone: string;
    selfRef: string;
    otherRef: string;
  };
  methodology: Array<{ name: string; description: string }>;
  mentalModels: Array<{ name: string; description: string }>;
  dialogueProtocols: {
    engaged: string[];
    cautious: string[];
    signature: string[];
  };
  knowledgeBoundary: {
    expert: string[];
    limited: string[];
  };
  redLines: string[];
  quotes: string[];
  systemPrompt: string;
}

export interface Character {
  id: string;
  name: string;
  title: string;
  era: string;
  avatar: string;
  tags: string[];
  description: string;
  systemPrompt: string;
  soulProfile?: SoulProfile;
  isPrebuilt?: boolean;
  createdAt: string;
  hasPixelSprites?: boolean; // flag — actual images live in IndexedDB
}

export interface Message {
  id: string;
  role: 'user' | 'assistant';
  content: string;
  timestamp: string;
  /**
   * Backend-typed reply segments from `metadata.output_contract`.
   *
   * Optional on purpose: the engine may omit it, and the renderer must then fall
   * back to `content` rather than showing an empty bubble.
   */
  outputContract?: ErosOutputContract | null;
  /** `done.generation_id`, when the turn was a real generation. */
  generationId?: string | null;
  /** `done.ghost_fallback` — the served reply resolved empty and was surfaced. */
  ghostFallback?: boolean;
  /** `meta.model` for the turn. */
  model?: string;
  /**
   * The user turn this assistant row answers (`history.user_message_id`).
   *
   * Retry/variant state is keyed by it: a turn's candidates all hang off the
   * same user message, and an assistant row on its own cannot identify them.
   */
  userMessageId?: string | null;
  /**
   * Engine-side marker carried over from history: `true` on a roleplay opening
   * anchor. Such a row is out of companion context and was never typed by the
   * user, so the message list filters it out of rendering.
   */
  opening?: boolean;
}

export type View = 'hall' | 'summon' | 'chat' | 'settings';
export type WizardStep = 1 | 2 | 3 | 4;
