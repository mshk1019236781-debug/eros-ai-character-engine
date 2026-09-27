/**
 * Model Settings adapter — Model Settings + Response Feedback V1, part A.
 *
 * Mirrors `crates/eros-engine-server/src/routes/settings.rs`. The engine holds
 * every API key; this client only ever sees `has_api_key` plus a masked tail.
 * There is deliberately no "reveal" call to make, and nothing in this file
 * writes a key to localStorage, sessionStorage or the console.
 */
import { EROS_BASE_URL } from './config.ts';
import { authHeaders, authToken } from './client.ts';

export type ErosModelPurpose = 'MAIN_RP' | 'CHARACTER_COMPILER';

export interface ErosModelProfile {
  purpose: string;
  provider: string;
  base_url: string;
  model_name: string;
  /** `profile` when the saved row is in force, `config` when the env wins. */
  source: string;
  has_api_key: boolean;
  api_key_masked?: string | null;
  temperature?: number | null;
  max_tokens?: number | null;
  is_active: boolean;
  updated_at?: string | null;
  /** What a generation would actually send right now. */
  effective_model: string;
}

export interface ErosModelSettings {
  profiles: ErosModelProfile[];
  /** Always `LOCAL_BACKEND_SECRET_FILE` in V1 — no pretend encryption. */
  secret_storage: string;
  secret_file: string;
}

export interface ErosProfilePatch {
  provider?: string;
  base_url?: string;
  model_name?: string;
  /**
   * Omit to keep the stored key. Sending the masked string is refused by the
   * engine, so "saved other fields while the key stayed put" cannot overwrite it.
   */
  api_key?: string | null;
  temperature?: number | null;
  max_tokens?: number | null;
  is_active?: boolean;
}

export interface ErosTestOutcome {
  success: boolean;
  provider: string;
  model: string;
  latency_ms: number;
  error_category?: string | null;
}

async function jsonRequest<T>(path: string, init: RequestInit): Promise<T> {
  const token = await authToken();
  const response = await fetch(`${EROS_BASE_URL}${path}`, {
    ...init,
    headers: { ...authHeaders(token), 'Content-Type': 'application/json', ...(init.headers ?? {}) },
  });
  if (!response.ok) {
    const detail = await response.text().catch(() => '');
    throw new Error(`settings ${response.status}${detail ? `: ${detail.slice(0, 200)}` : ''}`);
  }
  return (await response.json()) as T;
}

export function loadModelSettings(): Promise<ErosModelSettings> {
  return jsonRequest<ErosModelSettings>('/comp/settings/models', { method: 'GET' });
}

export function saveModelProfile(
  purpose: ErosModelPurpose,
  patch: ErosProfilePatch,
): Promise<ErosModelProfile> {
  return jsonRequest<ErosModelProfile>(`/comp/settings/models/${purpose}`, {
    method: 'PUT',
    body: JSON.stringify(patch),
  });
}

export function testModelProfile(purpose: ErosModelPurpose): Promise<ErosTestOutcome> {
  return jsonRequest<ErosTestOutcome>(`/comp/settings/models/${purpose}/test`, { method: 'POST' });
}
