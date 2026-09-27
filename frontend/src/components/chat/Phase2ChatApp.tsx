import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  EROS_TEST_CHARACTER,
  toPresentationCharacter,
} from '../../config/erosTestCharacter.ts';
import type { ErosTestCharacterConfig } from '../../config/erosTestCharacter.ts';
import {
  authToken,
  createSession,
  getCharacter,
  healthz,
  loadCharacter,
  loadHistory,
  loadSessions,
  sendMessage,
} from '../../lib/eros/client.ts';
import { EROS_SESSION_STORAGE_KEY } from '../../lib/eros/config.ts';
import {
  isHiddenHistoryRow,
  latestScene,
  latestStatusCard,
  mapErosMessage,
} from '../../lib/eros/mapper.ts';
import {
  avatarInitial,
  deriveSessionTitle,
  formatSessionTime,
} from '../../lib/eros/phase2View.ts';
import type {
  ErosCharacterLibraryEntry,
  ErosProfile,
  ErosRpMode,
  ErosSessionListEntry,
} from '../../lib/eros/types.ts';
import {
  loadVariants,
  retryVariant,
  selectVariant,
  setVariantLike,
} from '../../lib/eros/variants.ts';
import type { ErosVariantTurn } from '../../lib/eros/variants.ts';
import type { Character, Message } from '../../lib/types.ts';
import { ChatComposer } from './ChatComposer.tsx';
import { ChatHeader } from './ChatHeader.tsx';
import { ChatSidebar } from './ChatSidebar.tsx';
import type { SessionView } from './ChatSidebar.tsx';
import { MessageList } from './MessageList.tsx';
import type { VariantActions } from './MessageItem.tsx';
import { SceneIndicator } from './SceneIndicator.tsx';
import { StatusCard } from './StatusCard.tsx';
import './chat.css';
import { ACCENT, CANVAS, DANGER, F, FM, RAIL, SURFACE, TEXT, TEXT_FAINT, TEXT_MUTED } from './theme.ts';

const COLUMN_MAX = 760;
const FRIENDLY_ERROR = '连接失败，请重试。';

type BootPhase = 'booting' | 'ready' | 'error';

const LOCAL_USER_PREFIX = 'local-user-';
const LOCAL_ASSISTANT_PREFIX = 'local-assistant-';
const noop = () => {};

/**
 * Where one character's open session id is cached.
 *
 * The shipped character keeps the bare key, so an existing install resumes the
 * conversation it already had. Every other character gets its own slot: a
 * single shared key would let a newly created character resume under an old
 * character's name and history.
 */
function sessionStorageKey(genomeId: string): string {
  return genomeId === EROS_TEST_CHARACTER.genomeId
    ? EROS_SESSION_STORAGE_KEY
    : `${EROS_SESSION_STORAGE_KEY}:${genomeId}`;
}

/**
 * Phase 2 host — the manually playable chat shell.
 *
 * It owns exactly one piece of authoritative state: which EROS session is open.
 * Conversation content always comes from the engine; the client keeps a local
 * copy only while a turn is in flight, and never stores history in
 * localStorage. The single cached key is the session id, so a reload resumes
 * the same conversation from EROS.
 */
export function Phase2ChatApp({
  character: activeCharacter,
  library,
  archived,
  onSelectCharacter,
  onDeleteCharacter,
  onRestoreCharacter,
  onCreateCharacter,
  onOpenSettings,
}: {
  /** Which genome to open. Absent ⇒ the shipped test character. */
  character?: { genomeId: string; name: string };
  /** Every character this user owns, as the shell read it (§三). */
  library?: ErosCharacterLibraryEntry[];
  /** Archived characters the sidebar offers to restore (§六). */
  archived?: ErosCharacterLibraryEntry[];
  /** Switch to another character and reopen that character's conversation. */
  onSelectCharacter?: (entry: ErosCharacterLibraryEntry) => void;
  /** Archive one character. The sidebar has already confirmed twice (§五). */
  onDeleteCharacter?: (entry: ErosCharacterLibraryEntry) => void | Promise<void>;
  /** Put an archived character back. The library refetch follows it. */
  onRestoreCharacter?: (entry: ErosCharacterLibraryEntry) => void | Promise<void>;
  /** Opens the Character Builder. Absent ⇒ the sidebar entry is hidden. */
  onCreateCharacter?: () => void;
  /** Opens Model Settings. Absent ⇒ the sidebar entry is hidden. */
  onOpenSettings?: () => void;
} = {}) {
  // The genome is the identity; the display name is only a fallback until the
  // engine answers with `persona_name`.
  const config = useMemo<ErosTestCharacterConfig>(() => ({
    name: activeCharacter?.name?.trim() || EROS_TEST_CHARACTER.name,
    genomeId: activeCharacter?.genomeId?.trim() || EROS_TEST_CHARACTER.genomeId,
    instanceId: '',
    userId: EROS_TEST_CHARACTER.userId,
  }), [activeCharacter?.genomeId, activeCharacter?.name]);
  const storageKey = useMemo(() => sessionStorageKey(config.genomeId), [config.genomeId]);

  const [phase, setPhase] = useState<BootPhase>('booting');
  const [bootProblem, setBootProblem] = useState('');
  const [character, setCharacter] = useState<Character>(() =>
    toPresentationCharacter(config, null),
  );
  const [subtitle, setSubtitle] = useState('');
  // 剧情演绎 vs 微信聊天 is the genome's own `art_metadata.rp_mode`. The chat
  // shell reads it once so the reply contract's channels can be presented the
  // way that mode means them (P1-4/P1-5); it never re-decides the mode.
  const [rpMode, setRpMode] = useState<ErosRpMode | null>(null);
  const [sessionId, setSessionId] = useState('');
  const [instanceId, setInstanceId] = useState(config.instanceId ?? '');
  const [sessions, setSessions] = useState<SessionView[]>([]);
  const [messages, setMessages] = useState<Message[]>([]);
  const [input, setInput] = useState('');
  const [streaming, setStreaming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [historyLoading, setHistoryLoading] = useState(false);
  const [notice, setNotice] = useState('');
  const abortRef = useRef<AbortController | null>(null);
  // Set by the first bootstrap. See the effect below for why it exists.
  const bootedRef = useRef(false);
  // § four: an engine-written opening anchor is filtered out of the transcript,
  // so its presence is tracked separately — that flag is what stops a reload
  // from paying for a second opening scene.
  const openingSeenRef = useRef(false);

  // Retry / variant / like state. Keyed by the *user* turn, because a turn's
  // candidates are a set hanging off one user message rather than a property of
  // the assistant row that happens to be official right now.
  const [variantTurns, setVariantTurns] = useState<Record<string, ErosVariantTurn>>({});
  const [variantIndex, setVariantIndex] = useState<Record<string, number>>({});
  const [likedVariants, setLikedVariants] = useState<Record<string, boolean>>({});
  const [variantBusy, setVariantBusy] = useState(false);

  /**
   * Label the rows of one character's session list for display.
   *
   * `entries` is already narrowed to the current character's instance by the
   * caller — the engine's own list is per user, and V1 has more than one
   * character.
   */
  const deriveSessionViews = useCallback(
    async (activeId: string, entries: ErosSessionListEntry[]): Promise<SessionView[]> => {
      const ordered = [...entries].sort((a, b) =>
        (b.last_active_at ?? '').localeCompare(a.last_active_at ?? ''),
      );
      return Promise.all(
        ordered.map(async (entry) => {
        let title = entry.session_id === activeId ? '当前对话' : '新对话';
        if (entry.session_id !== activeId) {
          const recent = await loadHistory(entry.session_id, { limit: 20 }).catch(() => []);
          const firstUser = recent.find((row) => row.role === 'user');
          title = deriveSessionTitle(firstUser?.content);
        }
        return {
          sessionId: entry.session_id,
          title,
          time: formatSessionTime(entry.last_active_at),
          isActive: entry.session_id === activeId,
        };
        }),
      );
    }, []);

  /** The caller's sessions for one character instance, newest first. */
  const loadOwnSessions = useCallback(
    async (forInstanceId: string): Promise<ErosSessionListEntry[]> => {
      const listed = await loadSessions(config.userId).catch(() => []);
      return listed.filter((row) => row.instance_id === forInstanceId);
    }, [config.userId]);

  /**
   * Every candidate set and approval in the open session.
   *
   * Candidates live outside `chat_messages`, so history alone cannot tell the
   * client that a turn has three versions or which one runs. `focus` asks for
   * the newest candidate of one turn to be shown — used right after a retry,
   * where landing back on V1 would look like nothing happened.
   */
  const refreshVariants = useCallback(async (targetSessionId: string, focus?: string) => {
    const snapshot = await loadVariants(targetSessionId).catch((err: unknown) => {
      console.error('[phase2] variants load failed', err);
      return null;
    });
    if (!snapshot) return;

    const turns: Record<string, ErosVariantTurn> = {};
    for (const turn of snapshot.turns) turns[turn.user_message_id] = turn;
    setVariantTurns(turns);
    setLikedVariants(Object.fromEntries(
      snapshot.feedback
        .filter((row) => row.feedback_type === 'character_like')
        .map((row) => [row.variant_id, true]),
    ));
    setVariantIndex((prev) => {
      const next: Record<string, number> = {};
      for (const turn of snapshot.turns) {
        if (focus === turn.user_message_id) {
          next[turn.user_message_id] = Math.max(turn.variants.length - 1, 0);
          continue;
        }
        const selected = turn.variants.findIndex((row) => row.status === 'selected');
        const kept = prev[turn.user_message_id];
        next[turn.user_message_id] =
          typeof kept === 'number' && kept >= 0 && kept < turn.variants.length
            ? kept
            : (selected >= 0 ? selected : 0);
      }
      return next;
    });
  }, []);

  /** Replace local state with the engine's view of one session. */
  const openSession = useCallback(async (targetId: string, focus?: string) => {
    const server = await loadHistory(targetId);
    openingSeenRef.current = server.some(isHiddenHistoryRow);
    const mapped = server.filter((row) => !isHiddenHistoryRow(row)).map(mapErosMessage);
    setSessionId(targetId);
    setMessages(mapped);
    window.localStorage.setItem(storageKey, targetId);
    await refreshVariants(targetId, focus);
    return mapped;
  }, [storageKey, refreshVariants]);

  /**
   * Roleplay opening (§ four). The engine writes the scene-setting directive
   * itself, so this turn needs no user text and leaves no user row behind: the
   * composer stays untouched and the only thing that appears is the character's
   * opening scene. It rides the ordinary turn machinery — same session, same
   * streaming, same stop button — because an opening is an ordinary reply whose
   * directive the engine supplied.
   */
  const runOpeningScene = useCallback(async (targetId: string) => {
    const stamp = new Date().toISOString();
    const replyId = `${LOCAL_ASSISTANT_PREFIX}${Date.now()}`;
    setNotice('');
    setMessages((prev) => [
      ...prev,
      { id: replyId, role: 'assistant', content: '', timestamp: stamp },
    ]);
    setStreaming(true);
    abortRef.current = new AbortController();

    try {
      const result = await sendMessage({
        sessionId: targetId,
        content: '',
        opening: true,
        signal: abortRef.current.signal,
        onDelta: (_chunk, accumulated) => {
          setMessages((prev) =>
            prev.map((row) => (row.id === replyId ? { ...row, content: accumulated } : row)),
          );
        },
      });

      if (result.status === 'error') {
        console.error('[phase2] opening failed', result.error);
        setMessages((prev) => prev.filter((row) => row.id !== replyId));
        return;
      }

      if (result.status === 'aborted' && !result.text.trim()) {
        setMessages((prev) => prev.filter((row) => row.id !== replyId));
        return;
      }

      setMessages((prev) => prev.map((row) => (row.id === replyId
        ? {
            ...row,
            content: result.text,
            model: result.model,
            generationId: result.generationId ?? null,
            ghostFallback: Boolean(result.ghostFallback),
          }
        : row)));

      if (result.status === 'ok') {
        const server = await loadHistory(targetId).catch(() => null);
        if (server) {
          openingSeenRef.current = server.some(isHiddenHistoryRow);
          const mapped = server.filter((row) => !isHiddenHistoryRow(row)).map(mapErosMessage);
          const stored = [...mapped].reverse().find((row) => row.role === 'assistant' && row.content.trim());
          if (stored && stored.content.trim() === result.text.trim()) setMessages(mapped);
        }
        setSessions(
          await deriveSessionViews(targetId, await loadOwnSessions(instanceId)).catch(() => []),
        );
      }
    } catch (err: unknown) {
      console.error('[phase2] opening threw', err);
      setMessages((prev) => prev.filter((row) => row.id !== replyId));
    } finally {
      setStreaming(false);
    }
  }, [deriveSessionViews, instanceId, loadOwnSessions]);

  const bootstrap = useCallback(async () => {
    setPhase('booting');
    setBootProblem('');
    setNotice('');
    try {
      if (!(await healthz())) throw new Error('EROS /healthz did not answer');

      // Mint first: a missing dev-token service should fail here, not halfway
      // through the first sentence.
      await authToken({ force: true, userId: config.userId });

      // Resolving through `chat/start` is also how the instance id arrives:
      // the engine creates the caller's instance of this genome on first use,
      // and resumes the existing one afterwards.
      const started = await createSession({
        instanceId: config.instanceId || undefined,
        genomeId: config.genomeId || undefined,
        userId: config.userId,
      });
      setInstanceId(started.instance_id);

      // Only this character's sessions may be listed or resumed. The engine's
      // list is per user; without this narrowing a second character would open
      // on the first character's conversation and its history would leak into
      // the wrong chat.
      const mine = await loadOwnSessions(started.instance_id);
      const cached = window.localStorage.getItem(storageKey);
      const activeId = cached && mine.some((row) => row.session_id === cached)
        ? cached
        : started.session_id;

      const profile: ErosProfile | null = await loadCharacter(started.instance_id).catch(() => null);

      setCharacter(toPresentationCharacter(config, profile, started.persona_name ?? null));
      setSubtitle(describeProfile(profile));

      // The character's stored definition — the same row the engine's prompt
      // reads. Only `rp_mode` is used here; a miss is not fatal, the strip
      // simply keeps its shipped behaviour.
      const genome = await getCharacter(config.genomeId).catch(() => null);
      const mode = genome?.art_metadata?.rp_mode;
      setRpMode(mode === 'roleplay' || mode === 'chat' ? mode : null);

      const opened = await openSession(activeId);
      // A roleplay character opens on a scene, not on an empty box (§ four).
      // Only a session with nothing in it gets one: a resumed conversation
      // already has a beginning and must never grow a second.
      if (mode === 'roleplay' && !openingSeenRef.current && opened.length === 0) {
        await runOpeningScene(activeId);
      }
      setSessions(await deriveSessionViews(activeId, mine).catch(() => []));
      setPhase('ready');
    } catch (err: unknown) {
      console.error('[phase2] bootstrap failed', err);
      setBootProblem(err instanceof Error ? err.message : String(err));
      setPhase('error');
    }
  }, [config, storageKey, loadOwnSessions, deriveSessionViews, openSession, runOpeningScene]);

  // StrictMode mounts this effect twice in dev. Both runs resolved "no session
  // yet" and both asked `chat/start` for one, which is how a brand-new
  // character ended up with two empty conversations — the double window (§六).
  // The engine now serialises the two calls and the second one resumes instead
  // of creating, but the guard stops it being issued at all, so the create
  // path produces exactly one session. The retry button calls `bootstrap`
  // directly, so a failed boot can still be re-attempted.
  useEffect(() => {
    if (bootedRef.current) return;
    bootedRef.current = true;
    void bootstrap();
  }, [bootstrap]);

  const handleNewSession = useCallback(async () => {
    if (busy || streaming) return;
    setBusy(true);
    setNotice('');
    try {
      // A genuinely new engine session — never just a cleared message list.
      const started = await createSession({
        instanceId: config.instanceId || undefined,
        genomeId: config.genomeId || undefined,
        userId: config.userId,
        forceNew: true,
      });
      setInstanceId(started.instance_id);
      const opened = await openSession(started.session_id);
      if (rpMode === 'roleplay' && !openingSeenRef.current && opened.length === 0) {
        await runOpeningScene(started.session_id);
      }
      setSessions(
        await deriveSessionViews(
          started.session_id,
          await loadOwnSessions(started.instance_id),
        ).catch(() => []),
      );
    } catch (err: unknown) {
      console.error('[phase2] createSession failed', err);
      setNotice(FRIENDLY_ERROR);
    } finally {
      setBusy(false);
    }
  }, [busy, streaming, config, loadOwnSessions, deriveSessionViews, openSession, rpMode, runOpeningScene]);

  const handleSelectSession = useCallback(async (targetId: string) => {
    if (targetId === sessionId || streaming || busy) return;
    // A turn belongs to the session it was sent from; switching cancels it
    // rather than letting its reply land in the wrong conversation.
    abortRef.current?.abort();
    setBusy(true);
    setNotice('');
    setHistoryLoading(true);
    try {
      await openSession(targetId);
      setSessions((prev) => prev.map((row) => ({ ...row, isActive: row.sessionId === targetId })));
    } catch (err: unknown) {
      console.error('[phase2] session switch failed', err);
      setNotice(FRIENDLY_ERROR);
    } finally {
      setHistoryLoading(false);
      setBusy(false);
    }
  }, [sessionId, streaming, busy, openSession]);

  const handleSend = useCallback(async () => {
    const content = input.trim();
    if (!content || streaming || !sessionId) return;

    setInput('');
    setNotice('');
    const stamp = new Date().toISOString();
    const userMsg: Message = {
      id: `${LOCAL_USER_PREFIX}${Date.now()}`,
      role: 'user',
      content,
      timestamp: stamp,
    };
    const replyId = `${LOCAL_ASSISTANT_PREFIX}${Date.now()}`;
    const replyMsg: Message = { id: replyId, role: 'assistant', content: '', timestamp: stamp };

    setMessages((prev) => [...prev, userMsg, replyMsg]);
    setStreaming(true);
    abortRef.current = new AbortController();

    try {
      const result = await sendMessage({
        sessionId,
        content,
        signal: abortRef.current.signal,
        onDelta: (_chunk, accumulated) => {
          setMessages((prev) => prev.map((row) => (row.id === replyId ? { ...row, content: accumulated } : row)));
        },
      });

      if (result.status === 'error') {
        console.error('[phase2] turn failed', result.error);
        setNotice(FRIENDLY_ERROR);
        setMessages((prev) => prev.filter((row) => row.id !== replyId));
        return;
      }

      // Stopping before the first token leaves nothing to show.
      if (result.status === 'aborted' && !result.text.trim()) {
        setMessages((prev) => prev.filter((row) => row.id !== replyId));
        return;
      }

      setMessages((prev) => prev.map((row) => (row.id === replyId
        ? {
            ...row,
            content: result.text,
            model: result.model,
            generationId: result.generationId ?? null,
            ghostFallback: Boolean(result.ghostFallback),
          }
        : row)));

      // `output_contract` is attached at persistence time, so the typed segments
      // arrive with history. Reconcile only when the engine's stored assistant
      // turn matches what was just streamed; otherwise the streamed text stays
      // on screen rather than being replaced by something emptier.
      if (result.status === 'ok') {
        const server = await loadHistory(sessionId).catch(() => null);
        if (server) {
          openingSeenRef.current = server.some(isHiddenHistoryRow);
          const mapped = server.filter((row) => !isHiddenHistoryRow(row)).map(mapErosMessage);
          const stored = [...mapped].reverse().find((row) => row.role === 'assistant' && row.content.trim());
          if (stored && stored.content.trim() === result.text.trim()) setMessages(mapped);
        }
        setSessions(
          await deriveSessionViews(sessionId, await loadOwnSessions(instanceId)).catch(() => []),
        );
      }
    } catch (err: unknown) {
      console.error('[phase2] turn threw', err);
      setNotice(FRIENDLY_ERROR);
      setMessages((prev) => prev.filter((row) => row.id !== replyId));
    } finally {
      setStreaming(false);
    }
  }, [input, streaming, sessionId, instanceId, loadOwnSessions, deriveSessionViews]);

  const handleStop = useCallback(() => { abortRef.current?.abort(); }, []);

  /** Regenerate one user turn. Adds a candidate; never adds a user message. */
  const retryTurn = useCallback(async (userMessageId: string) => {
    if (!sessionId || streaming || variantBusy) return;
    setNotice('');
    setVariantBusy(true);
    try {
      const result = await retryVariant({ sessionId, userMessageId });
      if (result.status === 'error') {
        console.error('[phase2] retry failed', result.error);
        setNotice(result.error?.userMessage ?? FRIENDLY_ERROR);
      }
      await openSession(sessionId, userMessageId);
    } catch (err: unknown) {
      console.error('[phase2] retry threw', err);
      setNotice(FRIENDLY_ERROR);
    } finally {
      setVariantBusy(false);
    }
  }, [sessionId, streaming, variantBusy, openSession]);

  /** Adopt one candidate: the turn's official reply becomes this text. */
  const adoptVariant = useCallback(async (variantId: string) => {
    if (!sessionId || variantBusy) return;
    setNotice('');
    setVariantBusy(true);
    try {
      await selectVariant(sessionId, variantId);
      await openSession(sessionId);
    } catch (err: unknown) {
      console.error('[phase2] select failed', err);
      setNotice(FRIENDLY_ERROR);
    } finally {
      setVariantBusy(false);
    }
  }, [sessionId, variantBusy, openSession]);

  /**
   * Record or cancel "符合角色".
   *
   * The engine stores evidence and stops there: no review call, no exemplar,
   * no prompt change, and no extra model request from this click.
   */
  const toggleLike = useCallback(async (variantId: string) => {
    if (!sessionId || variantBusy) return;
    setVariantBusy(true);
    try {
      await setVariantLike(sessionId, variantId, !likedVariants[variantId]);
      await refreshVariants(sessionId);
    } catch (err: unknown) {
      console.error('[phase2] like failed', err);
      setNotice(FRIENDLY_ERROR);
    } finally {
      setVariantBusy(false);
    }
  }, [sessionId, variantBusy, likedVariants, refreshVariants]);

  /**
   * The transcript as it should be *shown*.
   *
   * Browsing a candidate swaps that row's text; the stored transcript is not
   * touched. A non-adopted candidate has no output contract of its own (the
   * contract is written onto the official row), so it renders as plain body.
   */
  const displayMessages = useMemo(() => messages.map((row) => {
    if (row.role !== 'assistant' || !row.userMessageId) return row;
    const turn = variantTurns[row.userMessageId];
    if (!turn || turn.variants.length === 0) return row;
    const shown = turn.variants[variantIndex[row.userMessageId] ?? 0];
    if (!shown || shown.status === 'selected') return row;
    return { ...row, content: shown.content, outputContract: null };
  }), [messages, variantTurns, variantIndex]);

  /** The reply a bare "重新生成" would target before any group exists. */
  const lastAssistantId = useMemo(() => {
    for (let i = messages.length - 1; i >= 0; i -= 1) {
      if (messages[i].role === 'assistant') return messages[i].id;
    }
    return null;
  }, [messages]);

  const variantActionsFor = useCallback((row: Message): VariantActions | undefined => {
    if (row.role !== 'assistant') return undefined;
    const userMessageId = row.userMessageId;
    if (!userMessageId) return undefined;
    const turn = variantTurns[userMessageId];
    if (!turn || turn.variants.length === 0) {
      // No candidate set exists yet. The engine back-fills candidate #1 from
      // this very reply the moment somebody retries, so "重新生成" is the one
      // affordance that means anything here — without it the *first* reply of
      // every turn would have no route into the variant UI at all.
      // 采用/👍 need a candidate row to point at, so they appear once the
      // group exists (i.e. after that first retry). Newest reply only, so a
      // long transcript is not peppered with control rows.
      if (row.id !== lastAssistantId) return undefined;
      return {
        index: 1,
        count: 1,
        ceiling: 1,
        status: 'selected',
        liked: false,
        canRetry: !streaming && !variantBusy,
        showLike: false,
        busy: variantBusy,
        onPrev: noop,
        onNext: noop,
        onSelect: noop,
        onRetry: () => void retryTurn(userMessageId),
        onToggleLike: noop,
      };
    }
    const count = turn.variants.length;
    // §13 draws the position as `‹ 1 / 3 ›`: the denominator is the per-turn
    // ceiling, not how many candidates happen to be loaded right now.
    const ceiling = count + Math.max(turn.remaining, 0);
    const index = variantIndex[userMessageId] ?? 0;
    const shown = turn.variants[index];
    if (!shown) return undefined;
    const step = (delta: number) => setVariantIndex((prev) => ({
      ...prev,
      [userMessageId]: (index + delta + count) % count,
    }));
    return {
      index: index + 1,
      count,
      ceiling,
      status: shown.status,
      liked: Boolean(likedVariants[shown.variant_id]),
      canRetry: turn.remaining > 0 && !streaming,
      showLike: true,
      busy: variantBusy,
      onPrev: () => step(-1),
      onNext: () => step(1),
      onSelect: () => void adoptVariant(shown.variant_id),
      onRetry: () => void retryTurn(userMessageId),
      onToggleLike: () => void toggleLike(shown.variant_id),
    };
  }, [variantTurns, variantIndex, likedVariants, streaming, variantBusy, lastAssistantId, adoptVariant, retryTurn, toggleLike]);

  if (phase !== 'ready') {
    return (
      <BootScreen
        title={phase === 'booting' ? '正在连接…' : '无法连接到角色服务'}
        detail={phase === 'error' ? FRIENDLY_ERROR : undefined}
        onRetry={phase === 'error' ? () => void bootstrap() : undefined}
        debug={bootProblem}
      />
    );
  }

  const scene = latestScene(messages);
  const statusCard = latestStatusCard(messages);

  return (
    <div style={{ height: '100vh', display: 'flex', background: SURFACE, overflow: 'hidden' }}>
      <ChatSidebar
        characterName={character.name}
        characterInitial={avatarInitial(character.name)}
        characterSubtitle={subtitle || undefined}
        characters={library}
        archived={archived}
        activeGenomeId={config.genomeId}
        onSelectCharacter={onSelectCharacter}
        onDeleteCharacter={onDeleteCharacter}
        onRestoreCharacter={onRestoreCharacter}
        sessions={sessions}
        onNewSession={() => void handleNewSession()}
        onCreateCharacter={onCreateCharacter}
        onOpenSettings={onOpenSettings}
        onSelectSession={(id) => void handleSelectSession(id)}
        busy={busy}
      />

      <main style={{
        flex: 1, minWidth: 0, height: '100%',
        display: 'flex', flexDirection: 'column',
        background: SURFACE,
      }}>
        <div style={{
          flex: 1, minHeight: 0,
          display: 'flex', flexDirection: 'column',
          width: '100%', maxWidth: COLUMN_MAX,
          margin: '0 auto', padding: '0 32px',
        }}>
          <ChatHeader characterName={character.name} subtitle={subtitle || undefined} />

          <div style={{
            flex: 1, minHeight: 0, display: 'flex', flexDirection: 'column',
            paddingTop: 4,
          }}>
            {scene && <SceneIndicator time={scene.time} location={scene.location} />}
            <StatusCard card={statusCard} roleplay={rpMode === 'roleplay'} />

            <MessageList
              messages={displayMessages}
              characterName={character.name}
              streaming={streaming}
              loading={historyLoading}
              emptyState={<EmptyState characterName={character.name} />}
              variantActionsFor={variantActionsFor}
            />

            {notice && (
              <div style={{
                flexShrink: 0, marginBottom: 8,
                fontFamily: F, fontSize: 12, color: DANGER,
                background: 'rgba(255,59,48,0.05)',
                border: '1px solid rgba(255,59,48,0.15)',
                borderRadius: 9, padding: '8px 12px',
              }}>{notice}</div>
            )}
          </div>

          <ChatComposer
            value={input}
            onChange={setInput}
            onSend={() => void handleSend()}
            onStop={handleStop}
            streaming={streaming}
            disabled={!sessionId}
            placeholder={`给${character.name}发消息…`}
          />
        </div>
      </main>
    </div>
  );
}

function describeProfile(profile: ErosProfile | null): string {
  if (!profile) return '';
  const parts: string[] = [];
  if (typeof profile.occupation === 'string' && profile.occupation.trim()) {
    parts.push(profile.occupation.trim());
  }
  if (typeof profile.location === 'string' && profile.location.trim()) {
    parts.push(profile.location.trim());
  }
  return parts.join(' · ');
}

/** Only ever the character and an invitation — this is RP, not a general assistant. */
function EmptyState({ characterName }: { characterName: string }) {
  return (
    <div data-testid="eros-empty-state" style={{ textAlign: 'center' }}>
      <div style={{
        fontFamily: F, fontSize: 17, color: TEXT,
        letterSpacing: '-0.01em', marginBottom: 6,
      }}>{characterName}</div>
      <div style={{ fontFamily: F, fontSize: 12.5, color: TEXT_MUTED }}>
        开始一段新的对话
      </div>
    </div>
  );
}

/**
 * Boot / failure screen.
 *
 * The reader sees one plain sentence; the underlying reason goes to the dev
 * console instead, so no stack trace, HTTP body or UUID reaches the UI.
 */
function BootScreen({ title, detail, onRetry, debug }: {
  title: string;
  detail?: string;
  onRetry?: () => void;
  debug?: string;
}) {
  useEffect(() => {
    if (debug) console.error('[phase2] boot detail', debug);
  }, [debug]);

  return (
    <div style={{
      height: '100vh', display: 'flex', alignItems: 'center', justifyContent: 'center',
      background: CANVAS, fontFamily: F,
    }}>
      <div style={{ textAlign: 'center', maxWidth: 360, padding: 24 }}>
        <div style={{ fontSize: 13.5, color: TEXT, marginBottom: detail ? 12 : 0 }}>{title}</div>
        {detail && <div style={{ fontSize: 12.5, color: TEXT_MUTED, lineHeight: 1.8 }}>{detail}</div>}
        {onRetry && (
          <button
            type="button"
            onClick={onRetry}
            style={{
              marginTop: 18, height: 32, padding: '0 20px', borderRadius: 9,
              border: `1px solid ${RAIL}`, background: '#FFFFFF',
              color: ACCENT, fontFamily: F, fontSize: 12.5, cursor: 'pointer',
            }}
          >重试</button>
        )}
        {!onRetry && (
          <div style={{
            marginTop: 14, fontFamily: FM, fontSize: 10.5,
            letterSpacing: '0.12em', color: TEXT_FAINT,
          }}>CONNECTING</div>
        )}
      </div>
    </div>
  );
}
