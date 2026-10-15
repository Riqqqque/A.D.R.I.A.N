export interface User {
  id: number;
  username: string;
  role: string;
}

export interface SetupFlags {
  provider_configured: boolean;
  ha_configured: boolean;
  lights_allowed: number;
  audio_endpoint_configured: boolean;
}

export interface MeResponse {
  user: User;
  csrf: string;
  settings: Record<string, string>;
  setup: SetupFlags;
}

export interface Fact {
  id: number;
  owner_id: number;
  owner: string;
  scope: string;
  text: string;
  source: string;
  created_at: number;
  updated_at: number;
  version: number;
}

export interface Entity {
  entity_id: string;
  name: string;
  area: string | null;
  domain: string;
  state: string;
  allowed: boolean;
  updated_at: number;
}

export interface Alarm {
  id: string;
  label: string;
  kind: string;
  timezone: string;
  local_time: string | null;
  recurrence: string;
  next_due_utc: number | null;
  endpoint_id: string | null;
  endpoint: string | null;
  volume: number;
  status: string;
  snooze_minutes: number;
  endpoint_name?: string | null;
  last_delivery_status?: string | null;
  last_delivery_detail?: string | null;
}

export interface Endpoint {
  id: string;
  name: string;
  kind: string;
  config: Record<string, unknown>;
  enabled: boolean;
}

export interface Provider {
  id: string;
  kind: string;
  label: string;
  base_url: string;
  model: string;
  enabled: boolean;
  is_default: boolean;
  has_key: boolean;
  timeout_secs: number;
  /** Loopback/LAN endpoint: free and never budget-limited. */
  local: boolean;
}

export interface HaStatus {
  configured: boolean;
  enabled?: boolean;
  url?: string;
  connected?: boolean;
  last_ok_at?: number | null;
  last_error?: string | null;
}

export interface Diagnostics {
  version: string;
  uptime_secs: number;
  db: { ok: boolean; sqlite_version?: string; journal_mode?: string; synchronous?: number };
  home_assistant: { configured: boolean; connected: boolean; last_error: string | null };
  providers: { id: string; label: string; kind: string; model: string; enabled: boolean; is_default: boolean }[];
  counts: Record<string, number>;
  warnings: string[];
}

export interface LedgerRow {
  id: number;
  user_id: number | null;
  actor: string;
  action: string;
  target: string;
  outcome: string;
  detail: string;
  created_at: number;
}

export interface UsageRow {
  provider_id: string;
  provider: string;
  model: string;
  capability: 'chat' | 'stt';
  module: string;
  calls: number;
  input_tokens: number;
  cached_tokens: number;
  output_tokens: number;
  audio_seconds: number;
  cost_micros: number;
  unpriced_calls: number;
  estimated_calls: number;
  local: boolean;
}

export interface Budget {
  id: string;
  scope: 'global' | 'provider' | 'module';
  scope_id: string;
  monthly_limit_micros: number;
  soft_pct: number;
  enabled: boolean;
  spent_micros: number;
}

export interface UsageSummary {
  month: string;
  total_cost_micros: number;
  rows: UsageRow[];
  budgets: Budget[];
  unpriced: string[];
}

export interface ModelPrice {
  provider_id: string;
  model: string;
  input_per_mtok_micros: number;
  cached_per_mtok_micros: number | null;
  output_per_mtok_micros: number;
  audio_per_minute_micros: number;
  updated_at: number;
}
