import { useCallback, useEffect, useState } from 'react';
import type { CSSProperties, ReactNode } from 'react';
import {
  loadModelSettings,
  saveModelProfile,
  testModelProfile,
} from '../../lib/eros/settings.ts';
import type {
  ErosModelProfile,
  ErosModelPurpose,
  ErosModelSettings,
  ErosProfilePatch,
} from '../../lib/eros/settings.ts';
import { ACCENT, CANVAS, DANGER, F, FM, RAIL, SURFACE, TEXT, TEXT_FAINT, TEXT_MUTED } from '../chat/theme.ts';

/**
 * Model settings — where the running engine reads its providers.
 *
 * The key lives in the engine's own secret file. This page can replace it, and
 * can show that one is configured, but there is no path back to the plaintext:
 * the masked tail is display-only, and the engine refuses a masked value as a
 * new key, so saving the other fields can never clobber the stored one.
 */
export function ModelSettingsPage({ onBack }: { onBack: () => void }) {
  const [settings, setSettings] = useState<ErosModelSettings | null>(null);
  const [problem, setProblem] = useState('');
  const [loading, setLoading] = useState(true);

  const reload = useCallback(async () => {
    setLoading(true);
    setProblem('');
    try {
      setSettings(await loadModelSettings());
    } catch (err: unknown) {
      console.error('[settings] load failed', err);
      setProblem('无法读取模型设置');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { void reload(); }, [reload]);

  const profileOf = (purpose: ErosModelPurpose): ErosModelProfile | undefined =>
    settings?.profiles.find((row) => row.purpose === purpose);

  return (
    <div style={{ minHeight: '100vh', background: CANVAS, fontFamily: F }}>
      <div style={{ maxWidth: 720, margin: '0 auto', padding: '28px 24px 56px' }}>
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 4 }}>
          <div style={{ fontSize: 17, color: TEXT, letterSpacing: '-0.01em' }}>模型设置</div>
          <button type="button" data-testid="eros-settings-back" onClick={onBack} style={ghostButton}>返回对话</button>
        </div>
        <div style={{ fontSize: 12, color: TEXT_MUTED, marginBottom: 22, lineHeight: 1.8 }}>
          更换模型不需要改源码或环境变量。API Key 只保存在后端，页面不会显示完整 Key，也不会写入浏览器存储。
        </div>

        {problem && (
          <div style={{ fontSize: 12, color: DANGER, background: 'rgba(255,59,48,0.05)', border: '1px solid rgba(255,59,48,0.15)', borderRadius: 9, padding: '8px 12px', marginBottom: 14 }}>{problem}</div>
        )}
        {loading && <div style={{ fontSize: 12.5, color: TEXT_MUTED }}>正在读取…</div>}

        {settings && (
          <>
            <ProfilePanel
              purpose="MAIN_RP"
              title="主 RP 模型"
              hint="角色回复与剧情推进所用的模型。切换后下一轮生成即生效，不迁移会话、不清记忆。"
              profile={profileOf('MAIN_RP')}
              onSaved={reload}
            />
            <ProfilePanel
              purpose="CHARACTER_COMPILER"
              title="角色分析模型"
              hint="角色编译器（导入角色卡、生成结构化档案）所用的模型，与主 RP 独立。"
              profile={profileOf('CHARACTER_COMPILER')}
              onSaved={reload}
            />
            <div style={{ marginTop: 18, fontFamily: FM, fontSize: 9.5, letterSpacing: '0.06em', color: TEXT_FAINT, lineHeight: 1.9 }}>
              SECRET_STORAGE = {settings.secret_storage}
              <br />
              {settings.secret_file}
            </div>
          </>
        )}
      </div>
    </div>
  );
}

function ProfilePanel({
  purpose,
  title,
  hint,
  profile,
  onSaved,
}: {
  purpose: ErosModelPurpose;
  title: string;
  hint: string;
  profile?: ErosModelProfile;
  onSaved: () => void;
}) {
  const [provider, setProvider] = useState('');
  const [baseUrl, setBaseUrl] = useState('');
  const [modelName, setModelName] = useState('');
  const [temperature, setTemperature] = useState('');
  const [maxTokens, setMaxTokens] = useState('');
  const [replacingKey, setReplacingKey] = useState(false);
  const [apiKey, setApiKey] = useState('');
  const [notice, setNotice] = useState('');
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!profile) return;
    setProvider(profile.provider);
    setBaseUrl(profile.base_url);
    setModelName(profile.model_name);
    setTemperature(profile.temperature == null ? '' : String(profile.temperature));
    setMaxTokens(profile.max_tokens == null ? '' : String(profile.max_tokens));
    setReplacingKey(false);
    setApiKey('');
  }, [profile]);

  const save = useCallback(async () => {
    setBusy(true);
    setError('');
    setNotice('');
    try {
      const patch: ErosProfilePatch = {
        provider: provider.trim(),
        base_url: baseUrl.trim(),
        model_name: modelName.trim(),
        is_active: true,
      };
      const temp = temperature.trim();
      const max = maxTokens.trim();
      patch.temperature = temp === '' ? null : Number(temp);
      patch.max_tokens = max === '' ? null : Number(max);
      // Only ever sent when the user actually typed a replacement. Sending the
      // masked tail would be refused by the engine anyway — this keeps the
      // intent explicit on this side too.
      if (replacingKey && apiKey.trim()) patch.api_key = apiKey.trim();

      const saved = await saveModelProfile(purpose, patch);
      setApiKey('');
      setReplacingKey(false);
      setNotice(`已保存 · 生效模型 ${saved.effective_model}`);
      onSaved();
    } catch (err: unknown) {
      console.error('[settings] save failed', err);
      setError('保存失败，请检查字段');
    } finally {
      setBusy(false);
    }
  }, [apiKey, baseUrl, maxTokens, modelName, onSaved, provider, purpose, replacingKey, temperature]);

  const test = useCallback(async () => {
    setBusy(true);
    setError('');
    setNotice('');
    try {
      const outcome = await testModelProfile(purpose);
      setNotice(outcome.success
        ? `连接正常 · ${outcome.provider} / ${outcome.model} · ${outcome.latency_ms} ms`
        : `连接失败 · ${outcome.error_category ?? 'unknown'}`);
    } catch (err: unknown) {
      console.error('[settings] test failed', err);
      setError('测试连接失败');
    } finally {
      setBusy(false);
    }
  }, [purpose]);

  return (
    <section
      data-testid={`eros-settings-${purpose}`}
      style={{ background: SURFACE, border: `1px solid ${RAIL}`, borderRadius: 14, padding: '16px 18px 18px', marginBottom: 16 }}
    >
      <div style={{ display: 'flex', alignItems: 'baseline', justifyContent: 'space-between' }}>
        <div style={{ fontSize: 13.5, color: TEXT }}>{title}</div>
        <div style={{ fontFamily: FM, fontSize: 9.5, letterSpacing: '0.08em', color: TEXT_FAINT }}>
          {profile ? `${profile.source.toUpperCase()} · ${profile.effective_model}` : ''}
        </div>
      </div>
      <div style={{ fontSize: 11.5, color: TEXT_MUTED, margin: '6px 0 14px', lineHeight: 1.75 }}>{hint}</div>

      <Field label="Provider">
        <input data-testid={`eros-provider-${purpose}`} value={provider} onChange={(e) => setProvider(e.target.value)} style={input} placeholder="deepseek" />
      </Field>
      <Field label="Base URL">
        <input data-testid={`eros-base-url-${purpose}`} value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} style={input} placeholder="https://api.deepseek.com/v1/chat/completions" />
      </Field>
      <Field label="Model Name">
        <input data-testid={`eros-model-name-${purpose}`} value={modelName} onChange={(e) => setModelName(e.target.value)} style={input} placeholder="deepseek-chat" />
      </Field>

      <Field label="API Key">
        {replacingKey ? (
          <div style={{ display: 'flex', gap: 8 }}>
            <input
              data-testid={`eros-api-key-${purpose}`}
              value={apiKey}
              onChange={(e) => setApiKey(e.target.value)}
              style={input}
              type="password"
              autoComplete="off"
              placeholder="粘贴新的 API Key"
            />
            <button type="button" onClick={() => { setReplacingKey(false); setApiKey(''); }} style={ghostButton}>取消</button>
          </div>
        ) : (
          <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
            <span data-testid={`eros-api-key-masked-${purpose}`} style={{ fontFamily: FM, fontSize: 11.5, color: profile?.has_api_key ? TEXT : TEXT_FAINT }}>
              {profile?.has_api_key ? `已配置：${profile.api_key_masked ?? 'sk-****'}` : '未配置'}
            </span>
            <button type="button" data-testid={`eros-replace-key-${purpose}`} onClick={() => setReplacingKey(true)} style={ghostButton}>
              替换 API Key
            </button>
          </div>
        )}
      </Field>

      <div style={{ display: 'flex', gap: 12 }}>
        <Field label="Temperature" style={{ flex: 1 }}>
          <input data-testid={`eros-temperature-${purpose}`} value={temperature} onChange={(e) => setTemperature(e.target.value)} style={input} placeholder="默认" />
        </Field>
        <Field label="Max Tokens" style={{ flex: 1 }}>
          <input data-testid={`eros-max-tokens-${purpose}`} value={maxTokens} onChange={(e) => setMaxTokens(e.target.value)} style={input} placeholder="默认" />
        </Field>
      </div>

      {notice && <div style={{ fontSize: 11.5, color: ACCENT, marginTop: 12 }}>{notice}</div>}
      {error && <div style={{ fontSize: 11.5, color: DANGER, marginTop: 12 }}>{error}</div>}

      <div style={{ display: 'flex', gap: 8, marginTop: 16 }}>
        <button
          type="button"
          data-testid={`eros-test-${purpose}`}
          onClick={() => void test()}
          disabled={busy}
          style={{ ...ghostButton, height: 32, padding: '0 16px' }}
        >测试连接</button>
        <button
          type="button"
          data-testid={`eros-save-${purpose}`}
          onClick={() => void save()}
          disabled={busy}
          style={{
            height: 32, padding: '0 18px', borderRadius: 9, border: 'none',
            background: busy ? TEXT_FAINT : ACCENT, color: '#FFFFFF',
            fontFamily: F, fontSize: 12.5, cursor: busy ? 'default' : 'pointer',
          }}
        >保存</button>
      </div>
    </section>
  );
}

function Field({ label, children, style }: { label: string; children: ReactNode; style?: CSSProperties }) {
  return (
    <label style={{ display: 'block', marginBottom: 12, ...style }}>
      <div style={{ fontFamily: FM, fontSize: 9, letterSpacing: '0.12em', color: TEXT_FAINT, marginBottom: 5, textTransform: 'uppercase' }}>{label}</div>
      {children}
    </label>
  );
}

const input: CSSProperties = {
  width: '100%', height: 33, boxSizing: 'border-box',
  border: `1px solid ${RAIL}`, borderRadius: 8,
  padding: '0 10px', fontFamily: F, fontSize: 12.5, color: TEXT,
  background: '#FFFFFF', outline: 'none',
};

const ghostButton: CSSProperties = {
  height: 28, padding: '0 12px', borderRadius: 8,
  border: `1px solid ${RAIL}`, background: '#FFFFFF',
  color: TEXT_MUTED, fontFamily: F, fontSize: 11.5, cursor: 'pointer',
  whiteSpace: 'nowrap',
};
