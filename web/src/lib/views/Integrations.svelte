<script lang="ts">
  import { del, get, post, put } from '../api';
  import { session } from '../session.svelte';
  import type { HaStatus, Provider } from '../types';

  let providers = $state<Provider[]>([]);
  let ha = $state<HaStatus | null>(null);
  let error = $state('');
  let notice = $state('');
  let testMsg = $state<Record<string, string>>({});

  // provider form
  let kind = $state('openai_responses');
  let plabel = $state('');
  let baseUrl = $state('https://api.openai.com/v1');
  let model = $state('gpt-5-mini');
  let apiKey = $state('');
  let timeoutSecs = $state(90);

  // ha form
  let haUrl = $state('');
  let haToken = $state('');
  const isOwner = $derived(session.user?.role === 'owner');

  const presets: Record<string, { label: string; url: string; model: string }> = {
    openai_responses: { label: 'OpenAI', url: 'https://api.openai.com/v1', model: 'gpt-5-mini' },
    deepseek: { label: 'DeepSeek', url: 'https://api.deepseek.com/v1', model: 'deepseek-chat' },
    local: { label: 'Local (llama.cpp / LM Studio / Ollama)', url: 'http://127.0.0.1:8080/v1', model: 'local' }
  };

  function applyPreset(k: string) {
    const p = presets[k];
    if (!p) return;
    kind = k === 'openai_responses' ? 'openai_responses' : 'openai_compatible';
    plabel = p.label;
    baseUrl = p.url;
    model = p.model;
  }

  async function load() {
    const [p, h] = await Promise.all([
      get<{ providers: Provider[] }>('/api/integrations/providers'),
      get<HaStatus>('/api/integrations/ha')
    ]);
    providers = p.providers;
    ha = h;
    if (h.url) haUrl = h.url;
  }
  load();

  async function addProvider() {
    error = '';
    try {
      await post('/api/integrations/providers', {
        kind, label: plabel, base_url: baseUrl, model,
        api_key: apiKey || undefined, timeout_secs: timeoutSecs
      });
      apiKey = '';
      await load();
      await session.refreshFlags();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function testProvider(p: Provider) {
    testMsg[p.id] = 'testing…';
    try {
      const r = await post<{ ok: boolean; detail: string }>(`/api/integrations/providers/${p.id}/test`, {});
      testMsg[p.id] = r.detail;
    } catch (e) {
      testMsg[p.id] = e instanceof Error ? e.message : String(e);
    }
  }

  async function setDefault(p: Provider) {
    await post(`/api/integrations/providers/${p.id}/default`, {});
    await load();
  }

  async function removeProvider(p: Provider) {
    if (!confirm(`Remove provider "${p.label}"?`)) return;
    await del(`/api/integrations/providers/${p.id}`);
    await load();
  }

  async function saveHa() {
    error = '';
    notice = '';
    try {
      await put('/api/integrations/ha', { url: haUrl, token: haToken, enabled: true });
      haToken = '';
      notice = 'Home Assistant connection saved.';
      await load();
      await session.refreshFlags();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function testHa() {
    error = '';
    notice = '';
    try {
      const r = await post<{ ok: boolean; detail: string }>('/api/integrations/ha/test', {});
      notice = r.detail;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function importEntities() {
    error = '';
    notice = '';
    try {
      const r = await post<{ imported: number }>('/api/integrations/ha/import', {});
      notice = `Imported ${r.imported} entities. Authorize lights under Devices.`;
      await load();
      await session.refreshFlags();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }
</script>

<h2>Integrations</h2>
{#if error}<div class="banner">{error}</div>{/if}
{#if notice}<div class="notice ok">{notice}</div>{/if}

<div class="panel">
  <h2>AI providers</h2>
  {#if providers.length === 0}<p class="dim">No providers yet — add one to enable chat.</p>{/if}
  {#each providers as p (p.id)}
    <div class="row" style="padding:.4rem 0; border-bottom:1px solid var(--border)">
      <span class="grow"><strong>{p.label}</strong> <span class="dim small">{p.kind} · {p.model} · {p.base_url}</span></span>
      {#if p.is_default}<span class="chip ok">default</span>{/if}
      {#if !p.enabled}<span class="chip">disabled</span>{/if}
      {#if isOwner}
        <button class="small ghost" onclick={() => testProvider(p)}>Test</button>
        {#if !p.is_default}<button class="small ghost" onclick={() => setDefault(p)}>Make default</button>{/if}
        <button class="small danger" onclick={() => removeProvider(p)}>Remove</button>
      {/if}
    </div>
    {#if testMsg[p.id]}<p class="small dim" style="margin:.2rem 0">{testMsg[p.id]}</p>{/if}
  {/each}
  {#if isOwner}
    <div class="row" style="margin-top:.8rem; align-items:end">
      <div><label>Preset</label>
        <select onchange={(e) => applyPreset((e.target as HTMLSelectElement).value)}>
          <option value="">choose…</option>
          <option value="openai_responses">OpenAI</option>
          <option value="deepseek">DeepSeek</option>
          <option value="local">Local server</option>
        </select>
      </div>
      <div><label>Label</label><input bind:value={plabel} /></div>
      <div class="grow"><label>Base URL</label><input style="width:100%" bind:value={baseUrl} /></div>
      <div><label>Model</label><input bind:value={model} /></div>
    </div>
    <div class="row" style="align-items:end">
      <div class="grow"><label>API key (stored server-side, optional for local)</label><input style="width:100%" type="password" bind:value={apiKey} autocomplete="off" /></div>
      <div><label>Timeout (s)</label><input type="number" min="5" max="600" style="width:6rem" bind:value={timeoutSecs} /></div>
      <button onclick={addProvider} disabled={!plabel.trim() || !baseUrl.trim() || !model.trim()}>Add provider</button>
    </div>
  {/if}
</div>

<div class="panel">
  <h2>Home Assistant</h2>
  {#if ha}
    <p class="row">
      <span class="chip {ha.connected ? 'ok' : ha.configured ? 'warn' : ''}">
        {ha.connected ? 'connected' : ha.configured ? 'configured, not connected' : 'not configured'}
      </span>
      {#if ha.last_error}<span class="err small">{ha.last_error}</span>{/if}
    </p>
  {/if}
  {#if isOwner}
    <div class="row" style="align-items:end">
      <div class="grow"><label>URL</label><input style="width:100%" bind:value={haUrl} placeholder="http://homeassistant.local:8123" /></div>
      <div class="grow"><label>Long-lived access token</label><input style="width:100%" type="password" bind:value={haToken} placeholder={ha?.configured ? '(keep existing — leave blank)' : ''} /></div>
      <button onclick={saveHa} disabled={!haUrl.trim() || (!haToken.trim() && !ha?.configured)}>Save</button>
    </div>
  {/if}
  <div class="row" style="margin-top:.6rem">
    <button class="ghost" onclick={testHa} disabled={!ha?.configured}>Test connection</button>
    <button class="ghost" onclick={importEntities} disabled={!ha?.connected}>Import entities</button>
  </div>
  <p class="dim small">Tokens stay server-side. Entity import only registers devices — nothing becomes controllable until you allowlist it under Devices.</p>
</div>
