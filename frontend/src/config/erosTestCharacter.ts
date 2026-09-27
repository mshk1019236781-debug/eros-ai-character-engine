import type { Character } from '../lib/types.ts';
import { EROS_DEV_USER_ID } from '../lib/eros/config.ts';
import type { ErosProfile } from '../lib/eros/types.ts';

/**
 * The single Phase 1 test character.
 *
 * This file holds IDs and presentation strings only. Prompt, character rules,
 * Expression Core, Response Contract, memory and world facts all stay on the
 * engine — nothing about how the character behaves is duplicated here.
 *
 * Resolution: `genome_id` is enough. `POST /comp/chat/start` resolves (or
 * auto-creates) the caller's persona instance for that genome and returns both
 * ids, so no fixture row has to be seeded by hand.
 */
export interface ErosTestCharacterConfig {
  /** Display name. The engine also returns `persona_name` on chat/start. */
  name: string;
  /** Genome to open a chat with. */
  genomeId: string;
  /** Optional explicit instance; empty means "let the engine resolve it". */
  instanceId?: string;
  /** Dev identity (`sub` claim) used to mint the local bearer token. */
  userId: string;
}

export const EROS_TEST_CHARACTER: ErosTestCharacterConfig = {
  name: '裴烬',
  genomeId: '23371efa-9eb6-4a5d-88d1-4bef0e251181',
  instanceId: '',
  userId: EROS_DEV_USER_ID,
};

/**
 * Minimal `Character` for the panel header/avatar.
 *
 * `systemPrompt` is intentionally empty: the old shell used it to prepend a
 * client-side persona prompt, and Phase 1 removed that path entirely.
 */
export function toPresentationCharacter(
  config: ErosTestCharacterConfig,
  profile: ErosProfile | null,
  personaName?: string | null,
): Character {
  const name = personaName?.trim() || config.name;
  return {
    id: config.instanceId || config.genomeId,
    name,
    title: typeof profile?.occupation === 'string' ? profile.occupation : '',
    era: typeof profile?.location === 'string' ? profile.location : 'EROS',
    avatar: '',
    tags: [],
    description: typeof profile?.current_situation === 'string' ? profile.current_situation : '',
    systemPrompt: '',
    createdAt: new Date().toISOString(),
  };
}
