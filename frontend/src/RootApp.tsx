import { useCallback, useEffect, useRef, useState } from 'react';
import { CharacterBuilderPage } from './components/character/CharacterBuilderPage.tsx';
import type { ActiveCharacter } from './components/character/CharacterBuilderPage.tsx';
import { CharacterLibraryHome } from './components/character/CharacterLibraryHome.tsx';
import { Phase2ChatApp } from './components/chat/Phase2ChatApp.tsx';
import { MultiActorChatApp } from './components/chat/multi/MultiActorChatApp.tsx';
import { ModelSettingsPage } from './components/settings/ModelSettingsPage.tsx';
import {
  archiveCharacter, loadArchivedLibrary, loadCharacterLibrary, restoreCharacter,
} from './lib/eros/client.ts';
import { EROS_DEV_USER_ID } from './lib/eros/config.ts';
import type { ErosCharacterLibraryEntry } from './lib/eros/types.ts';

/**
 * The shell above the chat: which character, and whether the builder is open.
 *
 * It owns the character library (§三). The list is read from the engine on
 * mount and is the only authority on which characters exist; `localStorage`
 * merely remembers which one was selected. A remembered genome that is no
 * longer in the library is replaced by the newest one, which is why a character
 * can no longer "disappear" when another is created.
 *
 * Deliberately a couple of `useState` values rather than a router: the whole
 * surface is one chat plus one form. Nothing here creates a session — that is
 * `Phase2ChatApp`'s boot, which resumes the character's existing conversation.
 */

/** The character the shell opens with until somebody picks or creates another. */
export const ACTIVE_CHARACTER_STORAGE_KEY = 'eros_active_character';

/**
 * Which surface the shell was on.
 *
 * The multi-actor view keeps its own conversation id, so without this a reload
 * would drop the user back into the single-character chat even though the
 * conversation they were in is still there.
 */
export const ACTIVE_VIEW_STORAGE_KEY = 'eros_active_view';

/**
 * Whether the library page is where the user chose to be.
 *
 * `libraryHomeRef` alone lasts only as long as the tab: deleting the character
 * on screen parked on the library, but the next reload fell back to the newest
 * character. Persisting the intent keeps case B of the lifecycle spec true
 * across a refresh, and `openCharacter` clears it the moment somebody actually
 * picks a character (case C).
 */
export const LIBRARY_HOME_STORAGE_KEY = 'eros_library_home_explicit';

/** The remembered "I picked the library" flag. Only an exact `true` counts. */
function readLibraryHomeExplicit(): boolean {
  try {
    return window.localStorage.getItem(LIBRARY_HOME_STORAGE_KEY) === 'true';
  } catch {
    return false;
  }
}

/**
 * Read the remembered character.
 *
 * Persisted so a reload lands on the character you were talking to. Without it
 * every refresh would fall back to the shipped 裴烬, which is exactly the
 * "创建完刷新又变回去" failure the builder has to avoid.
 *
 * `null` means "nothing remembered": the shell then lands on the newest
 * character (below), or on the library page when there is no character to
 * open at all — never on a chat window for a character the user did not pick.
 */
function readActiveCharacter(): ActiveCharacter | null {
  try {
    const raw = window.localStorage.getItem(ACTIVE_CHARACTER_STORAGE_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Partial<ActiveCharacter> | null;
    const genomeId = typeof parsed?.genomeId === 'string' ? parsed.genomeId.trim() : '';
    const name = typeof parsed?.name === 'string' ? parsed.name.trim() : '';
    if (genomeId && name) return { genomeId, name };
  } catch {
    // A corrupt entry is dropped rather than honoured.
  }
  return null;
}

/** The remembered surface. The builder is a task, never a landing page. */
function readActiveView(): 'chat' | 'multi' {
  try {
    return window.localStorage.getItem(ACTIVE_VIEW_STORAGE_KEY) === 'multi' ? 'multi' : 'chat';
  } catch {
    return 'chat';
  }
}

export function RootApp() {
  const [library, setLibrary] = useState<ErosCharacterLibraryEntry[]>([]);
  // The archived half of the library (§六). Kept next to the live list so the
  // two can never disagree about what exists.
  const [archived, setArchived] = useState<ErosCharacterLibraryEntry[]>([]);
  const [libraryReady, setLibraryReady] = useState(false);
  const [active, setActive] = useState<ActiveCharacter | null>(readActiveCharacter);
  const [view, setView] = useState<'chat' | 'builder' | 'multi' | 'settings'>(readActiveView);
  // Set while the shell is parked on the library page (§五). Without it the
  // reconciliation effect below would see "no active character" and open the
  // newest one the moment a character is deleted.
  // Seeded from storage so "I chose the library" survives a reload, not just
  // the tab. `openCharacter` and the delete flow below keep it in sync.
  const libraryHomeRef = useRef(readLibraryHomeExplicit());

  const rememberView = useCallback((next: 'chat' | 'multi') => {
    try { window.localStorage.setItem(ACTIVE_VIEW_STORAGE_KEY, next); } catch { /* private mode */ }
    setView(next);
  }, []);

  const openCharacter = useCallback((next: ActiveCharacter) => {
    libraryHomeRef.current = false;
    try {
      window.localStorage.setItem(ACTIVE_CHARACTER_STORAGE_KEY, JSON.stringify(next));
      // Case C: picking a character ends the "I am on the library" state.
      window.localStorage.removeItem(LIBRARY_HOME_STORAGE_KEY);
    } catch { /* private mode */ }
    setActive(next);
    rememberView('chat');
  }, [rememberView]);

  /**
   * Re-read the library from the engine.
   *
   * Returns the fresh rows as well as storing them, so a caller that just
   * created or deleted a character can act on the new list in the same tick
   * instead of guessing what it now contains.
   */
  const refreshLibrary = useCallback(async (): Promise<ErosCharacterLibraryEntry[]> => {
    try {
      const rows = await loadCharacterLibrary({ userId: EROS_DEV_USER_ID });
      setLibrary(rows);
      return rows;
    } catch (err: unknown) {
      // A library that cannot be read must not take the chat down with it: the
      // remembered character still boots and the sidebar simply stays empty.
      console.error('[root] character library failed', err);
      return [];
    } finally {
      setLibraryReady(true);
    }
  }, []);

  /** Re-read the archived list. Same contract as `refreshLibrary`. */
  const refreshArchived = useCallback(async (): Promise<ErosCharacterLibraryEntry[]> => {
    try {
      const rows = await loadArchivedLibrary({ userId: EROS_DEV_USER_ID });
      setArchived(rows);
      return rows;
    } catch (err: unknown) {
      // An unreadable archived list must not take the live one down with it.
      console.error('[root] archived library failed', err);
      return [];
    }
  }, []);

  useEffect(() => {
    void (async () => {
      const rows = await refreshLibrary();
      void refreshArchived();
      // The remembered genome is a hint, not the truth (§三). Keeping it when
      // it is still present is the no-flicker path; otherwise land on the
      // newest character. An empty library parks on the library page (§五).
      setActive((prev) => {
        if (libraryHomeRef.current) return null;
        const kept = prev ? rows.find((row) => row.genome_id === prev.genomeId) : undefined;
        if (kept && prev) {
          return kept.name === prev.name ? prev : { genomeId: kept.genome_id, name: kept.name };
        }
        const first = rows[0];
        return first ? { genomeId: first.genome_id, name: first.name } : null;
      });
    })();
  }, [refreshArchived, refreshLibrary]);

  /** The builder wrote a genome and its instance; the library now lists it. */
  const handleCreated = useCallback(async (created: ActiveCharacter) => {
    await refreshLibrary();
    openCharacter(created);
  }, [openCharacter, refreshLibrary]);

  const handleSelectCharacter = useCallback((entry: ErosCharacterLibraryEntry) => {
    if (entry.genome_id === active?.genomeId) return;
    openCharacter({ genomeId: entry.genome_id, name: entry.name });
  }, [active?.genomeId, openCharacter]);

  /**
   * Delete one character (§五). The sidebar already asked twice.
   *
   * The engine archives the instance and its conversations; the row is removed
   * from the sidebar because the refetched library no longer lists it, not
   * because the client filtered it locally.
   */
  const handleDeleteCharacter = useCallback(async (entry: ErosCharacterLibraryEntry) => {
    await archiveCharacter(entry.genome_id, { userId: EROS_DEV_USER_ID });
    await refreshLibrary();
    await refreshArchived();
    if (entry.genome_id !== active?.genomeId) return;
    // §五: the deleted character was the one on screen. Park on the library —
    // no other character is opened for the user, and no session is created.
    libraryHomeRef.current = true;
    try {
      window.localStorage.removeItem(ACTIVE_CHARACTER_STORAGE_KEY);
      window.localStorage.removeItem(ACTIVE_VIEW_STORAGE_KEY);
      // Case B: the park is a decision, so a reload must honour it too.
      window.localStorage.setItem(LIBRARY_HOME_STORAGE_KEY, 'true');
    } catch { /* private mode */ }
    setActive(null);
  }, [active?.genomeId, refreshArchived, refreshLibrary]);

  /**
   * Put an archived character back (§六), then open it.
   *
   * Opening is the point: restore revived the conversations the archive hid, so
   * the chat that mounts resumes the same session the user had before deleting
   * — not a fresh one. The engine is the one that says how much came back.
   */
  const handleRestoreCharacter = useCallback(async (entry: ErosCharacterLibraryEntry) => {
    await restoreCharacter(entry.genome_id, { userId: EROS_DEV_USER_ID });
    await refreshLibrary();
    await refreshArchived();
    openCharacter({ genomeId: entry.genome_id, name: entry.name });
  }, [openCharacter, refreshArchived, refreshLibrary]);

  if (view === 'builder') {
    return (
      <CharacterBuilderPage
        onCreated={(created) => void handleCreated(created)}
        onMultiStarted={() => rememberView('multi')}
      />
    );
  }

  // Model Settings is a task like the builder: transient, and never a landing
  // page a reload can strand somebody in.
  if (view === 'settings') {
    return <ModelSettingsPage onBack={() => setView('chat')} />;
  }

  if (view === 'multi') {
    // A second surface, not a second chat implementation: the multi-actor host
    // talks to a conversation over the same adapter the single chat uses.
    return <MultiActorChatApp onBackToSingle={() => rememberView('chat')} />;
  }

  // The library is read before the chat mounts, so the shell never boots one
  // character and then immediately remounts onto another after a reload.
  if (!libraryReady) {
    return (
      <div style={{
        height: '100vh', display: 'flex', alignItems: 'center', justifyContent: 'center',
        background: '#FFFFFF', color: '#8A8A8E', fontSize: 12.5,
      }}>正在读取角色…</div>
    );
  }

  // No character is open: the library itself is the screen (§五/§六). Nothing
  // here talks to the engine, so landing here can never create a session.
  if (!active) {
    return (
      <CharacterLibraryHome
        characters={library}
        archived={archived}
        onOpenCharacter={handleSelectCharacter}
        onCreateCharacter={() => setView('builder')}
        onRestoreCharacter={handleRestoreCharacter}
      />
    );
  }

  return (
    // Keyed by genome so switching characters remounts the chat shell instead
    // of leaving the previous character's messages on screen while the new
    // session boots.
    <Phase2ChatApp
      key={active.genomeId}
      character={active}
      library={library}
      archived={archived}
      onSelectCharacter={handleSelectCharacter}
      onDeleteCharacter={handleDeleteCharacter}
      onRestoreCharacter={handleRestoreCharacter}
      onCreateCharacter={() => setView('builder')}
      onOpenSettings={() => setView('settings')}
    />
  );
}
