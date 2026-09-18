<script lang="ts">
  import { get, post, put } from '../api';
  import { session } from '../session.svelte';
  import type { Diagnostics, LedgerRow } from '../types';

  let diag = $state<Diagnostics | null>(null);
  let ledger = $state<LedgerRow[]>([]);
  let error = $state('');
  let notice = $state('');
  const isOwner = $derived(session.user?.role === 'owner');

  const timezones: string[] =
    'supportedValuesOf' in Intl ? (Intl.supportedValuesOf('timeZone') as string[]) : ['UTC'];
  let timezone = $state('');
  let assistantName = $state('');
  let privacy = $state('hybrid');
  let publicUrl = $state('');

  async function load() {
    try {
      diag = await get<Diagnostics>('/api/diagnostics');
      if (isOwner) {
        const l = await get<{ entries: LedgerRow[] }>('/api/diagnostics/ledger');
        ledger = l.entries;
      }
      timezone = session.settings.timezone ?? 'UTC';
      assistantName = session.settings.assistant_name ?? 'Adrian';
      privacy = session.settings.privacy_mode ?? 'hybrid';
      publicUrl = session.settings.public_url ?? '';
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }
  load();

  async function saveSettings() {
    error = '';
    notice = '';
    try {
      await put('/api/settings', {
        timezone,
        assistant_name: assistantName,
        privacy_mode: privacy,
        public_url: publicUrl || null
      });
      notice = 'Settings saved.';
      await session.refreshFlags();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function backup() {
    error = '';
    notice = '';
    try {
      const r = await post<{ backup: string }>('/api/diagnostics/backup', {});
      notice = `Backup written to ${r.backup}`;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  function fmt(ts: number) {
    return new Date(ts * 1000).toLocaleString();
  }
  function uptime(s: number) {
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    return h ? `${h}h ${m}m` : `${m}m ${s % 60}s`;
  }
</script>

<h2>System</h2>
{#if error}<div class="banner">{error}</div>{/if}
{#if notice}<div class="notice ok">{notice}</div>{/if}

{#if diag}
  {#if diag.warnings.length}
    <div class="panel">
      <h2>Needs attention</h2>
      {#each diag.warnings as w}<p class="warn small" style="margin:.2rem 0">· {w}</p>{/each}
    </div>
  {/if}

  <div class="panel">
    <h2>Status</h2>
    <div class="row">
      <span class="chip ok">v{diag.version}</span>
      <span class="chip">up {uptime(diag.uptime_secs)}</span>
      <span class="chip {diag.db.ok ? 'ok' : 'err'}">sqlite {diag.db.sqlite_version} · {diag.db.journal_mode} · sync={diag.db.synchronous}</span>
      <span class="chip {diag.home_assistant.connected ? 'ok' : ''}">home assistant: {diag.home_assistant.connected ? 'connected' : diag.home_assistant.configured ? 'configured' : 'not configured'}</span>
    </div>
    <table style="margin-top:.8rem">
      <tbody>
        {#each Object.entries(diag.counts) as [k, v]}
          <tr><td class="dim">{k.replaceAll('_', ' ')}</td><td>{v}</td></tr>
        {/each}
      </tbody>
    </table>
    {#if isOwner}<div style="margin-top:.8rem"><button class="ghost" onclick={backup}>Write backup now</button></div>{/if}
  </div>
{/if}

{#if isOwner}
  <div class="panel">
    <h2>Settings</h2>
    <div class="row" style="align-items:end">
      <label class="field">Timezone
        <select bind:value={timezone}>{#each timezones as tz}<option value={tz}>{tz}</option>{/each}</select>
      </label>
      <label class="field">Assistant name<input bind:value={assistantName} /></label>
      <label class="field">Privacy
        <select bind:value={privacy}>
          <option value="hybrid">hybrid</option><option value="strict_local">strict-local</option>
        </select>
      </label>
      <label class="field grow">Public URL (for speakers to fetch audio)
        <input bind:value={publicUrl} placeholder="http://192.168.1.10:8787" />
      </label>
      <button onclick={saveSettings}>Save</button>
    </div>
    <p class="dim small">strict-local blocks cloud provider calls. Public URL should be reachable by Home Assistant media players.</p>
  </div>

  <div class="panel">
    <h2>Action ledger</h2>
    {#if ledger.length === 0}<p class="dim">No actions recorded yet.</p>{/if}
    <table>
      <thead><tr><th>When</th><th>Actor</th><th>Action</th><th>Target</th><th>Outcome</th><th>Detail</th></tr></thead>
      <tbody>
        {#each ledger as r (r.id)}
          <tr>
            <td class="small dim">{fmt(r.created_at)}</td>
            <td>{r.actor}</td>
            <td>{r.action}</td>
            <td class="small"><code>{r.target}</code></td>
            <td><span class="chip {r.outcome === 'confirmed' || r.outcome === 'ok' || r.outcome === 'scheduled' ? 'ok' : r.outcome === 'denied' || r.outcome === 'failed' ? 'err' : ''}">{r.outcome}</span></td>
            <td class="small dim">{r.detail}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  </div>
{/if}
