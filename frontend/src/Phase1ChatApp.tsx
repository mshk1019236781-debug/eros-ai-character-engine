import { useCallback, useEffect, useState } from 'react';
import type { ReactNode } from 'react';
import { PetChatPanel } from './components/pet/PetChatPanel.tsx';
import { EROS_TEST_CHARACTER, toPresentationCharacter } from './config/erosTestCharacter.ts';
import {
  authToken,
  createSession,
  healthz,
  loadCharacter,
  loadHistory,
} from './lib/eros/client.ts';
import { EROS_SESSION_STORAGE_KEY } from './lib/eros/config.ts';
import type { Character } from './lib/types.ts';
import type { ErosProfile } from './lib/eros/types.ts';

const F  = `-apple-system,'PingFang SC','Microsoft YaHei',system-ui,sans-serif`;
const FM = `'SF Mono','Roboto Mono',ui-monospace,monospace`;

type BootPhase = 'booting' | 'ready' | 'error';

function BootScreen({ title, detail, action }: { title: string; detail?: string; action?: ReactNode }) {
  return (
    <div style={{
      height: '100vh', display: 'flex', alignItems: 'center', justifyContent: 'center',
      background: '#F5F5F7', fontFamily: F,
    }}>
      <div style={{ maxWidth: 480, padding: 24, textAlign: 'center' }}>
        <div style={{ fontSize: 13.5, color: '#1D1D1F', letterSpacing: '-0.01em' }}>{title}</div>
        {detail && (
          <div style={{
            marginTop: 10, fontFamily: FM, fontSize: 11, lineHeight: 1.7,
            color: '#FF3B30', wordBreak: 'break-word', whiteSpace: 'pre-wrap',
          }}>{detail}</div>
        )}
        {action && <div style={{ marginTop: 16 }}>{action}</div>}
      </div>
    </div>
  );
}

/**
 * Phase 1 host.
 *
 * One fixed test character, one EROS session, the existing chat panel as the
 * only UI. Everything it shows comes from the engine: the session, the history,
 * the persona profile and the reply contract. The client contributes an id, a
 * display name and nothing else.
 */
export function Phase1ChatApp() {
  const [phase, setPhase]       = useState<BootPhase>('booting');
  const [problem, setProblem]   = useState('');
  const [sessionId, setSession] = useState('');
  const [character, setCharacter] = useState<Character>(() =>
    toPresentationCharacter(EROS_TEST_CHARACTER, null),
  );

  const bootstrap = useCallback(async () => {
    setPhase('booting');
    setProblem('');
    try {
      if (!(await healthz())) {
        throw new Error('EROS engine is not answering /healthz. Start it, then retry.');
      }

      // Mint before anything else, so a missing dev token service fails here
      // rather than halfway through the first sentence.
      await authToken({ force: true, userId: EROS_TEST_CHARACTER.userId });

      let activeSession = '';
      let instanceId = EROS_TEST_CHARACTER.instanceId ?? '';
      let personaName: string | null = null;

      // V1 front-end cache only: the session lives in the engine, we just remember
      // which one to reopen. A stale id is dropped the moment history refuses it.
      const cached = window.localStorage.getItem(EROS_SESSION_STORAGE_KEY);
      if (cached) {
        try {
          await loadHistory(cached, { limit: 1 });
          activeSession = cached;
        } catch {
          window.localStorage.removeItem(EROS_SESSION_STORAGE_KEY);
        }
      }

      if (!activeSession) {
        const started = await createSession({
          instanceId: EROS_TEST_CHARACTER.instanceId || undefined,
          genomeId: EROS_TEST_CHARACTER.genomeId || undefined,
          userId: EROS_TEST_CHARACTER.userId,
        });
        activeSession = started.session_id;
        instanceId = started.instance_id;
        personaName = started.persona_name ?? null;
        window.localStorage.setItem(EROS_SESSION_STORAGE_KEY, activeSession);
      }

      const profile: ErosProfile | null = instanceId
        ? await loadCharacter(instanceId).catch(() => null)
        : null;

      setCharacter(toPresentationCharacter(EROS_TEST_CHARACTER, profile, personaName));
      setSession(activeSession);
      setPhase('ready');
    } catch (err: unknown) {
      setProblem(err instanceof Error ? err.message : String(err));
      setPhase('error');
    }
  }, []);

  useEffect(() => { void bootstrap(); }, [bootstrap]);

  const handleStreamingChange = useCallback(() => {}, []);

  if (phase !== 'ready') {
    return (
      <BootScreen
        title={phase === 'booting' ? '正在连接 EROS Engine…' : 'EROS 连接失败'}
        detail={problem || undefined}
        action={phase === 'error' ? (
          <button
            onClick={() => void bootstrap()}
            style={{
              padding: '6px 16px', height: 30, borderRadius: 8,
              border: '1px solid rgba(0,0,0,0.12)', background: '#FFFFFF',
              color: '#1D1D1F', fontFamily: F, fontSize: 12, cursor: 'pointer',
            }}
          >重试</button>
        ) : undefined}
      />
    );
  }

  return (
    <div style={{
      height: '100vh', background: '#F5F5F7',
      display: 'flex', justifyContent: 'center', overflow: 'hidden',
    }}>
      <div style={{
        height: '100%', width: '100%', maxWidth: 820,
        background: '#FFFFFF', boxShadow: '0 0 0 1px rgba(0,0,0,0.05)',
      }}>
        <PetChatPanel
          character={character}
          sessionId={sessionId}
          width="100%"
          onStreamingChange={handleStreamingChange}
        />
      </div>
    </div>
  );
}
