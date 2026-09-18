<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { get, post, del } from '../api';
  import { session } from '../session.svelte';
  import type { Entity } from '../types';

  let entities = $state<Entity[]>([]);
  let error = $state('');
  let busy = $state('');
  let brightness = $state<Record<string, number>>({});
  let events: EventSource | null = null;
  const isOwner = $derived(session.user?.role === 'owner');

  async function load() {
    try {
      const res = await get<{ entities: Entity[] }>('/api/devices/entities');
      entities = res.entities;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  onMount(() => {
    load();
    // Live state updates pushed by the HA state subscription.
    events = new EventSource('/api/events');
    events.addEventListener('entity_state', (e) => {
      try {
        const d = JSON.parse((e as MessageEvent).data) as { entity_id: string; state: string };
        const ent = entities.find((x) => x.entity_id === d.entity_id);
        if (ent) ent.state = d.state;
      } catch { /* ignore */ }
    });
    events.addEventListener('entities', () => void load());
  });
  onDestroy(() => events?.close());

  async function setAllowed(e: Entity, allowed: boolean) {
    try {
      await post('/api/devices/entities/allow', { entity_id: e.entity_id, allowed });
      e.allowed = allowed;
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    }
  }

  async function command(e: Entity, action: string) {
    busy = e.entity_id;
    error = '';
    try {
      const res = await post<{ confirmed: boolean; detail: string }>(
        `/api/devices/entities/${encodeURIComponent(e.entity_id)}/command`,
        { action, brightness_pct: brightness[e.entity_id] }
      );
      e.state = res.confirmed ? (action === 'turn_off' ? 'off' : 'on') : e.state;
      if (!res.confirmed) error = `${e.name}: ${res.detail}`;
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      busy = '';
    }
  }

  // ---------- satellites ----------

  interface Satellite {
    id: string;
    name: string;
    paired_at: number;
    last_seen: number | null;
    revoked: boolean;
  }
  let satellites = $state<Satellite[]>([]);
  let pairCode = $state('');
  let pairExpires = $state(0);

  async function loadSatellites() {
    try {
      const res = await get<{ satellites: Satellite[] }>('/api/satellites');
      satellites = res.satellites;
    } catch { /* satellites are best-effort */ }
  }

  async function mintCode() {
    error = '';
    try {
      const res = await post<{ code: string; expires_at: number }>(
        '/api/satellites/pairing-code',
        {}
      );
      pairCode = res.code;
      pairExpires = res.expires_at;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function revokeSat(id: string) {
    error = '';
    try {
      await del(`/api/satellites/${encodeURIComponent(id)}`);
      await loadSatellites();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  onMount(() => void loadSatellites());
</script>

<h2>Devices</h2>
{#if error}<div class="banner">{error}</div>{/if}
{#if entities.length === 0}
  <div class="notice">
    No entities imported yet. Connect Home Assistant under
    <strong>Integrations</strong> and run <em>Import entities</em>.
  </div>
{:else}
  <div class="panel">
    <table>
      <thead><tr><th>Name</th><th>Entity</th><th>Area</th><th>State</th><th>Allowed</th><th></th></tr></thead>
      <tbody>
        {#each entities as e (e.entity_id)}
          <tr>
            <td>{e.name}</td>
            <td><code class="small">{e.entity_id}</code></td>
            <td>{e.area ?? '—'}</td>
            <td><span class="chip {e.state === 'on' ? 'ok' : ''}">{e.state || '?'}</span></td>
            <td>
              {#if isOwner}
                <input
                  type="checkbox"
                  checked={e.allowed}
                  onchange={(ev) => setAllowed(e, (ev.target as HTMLInputElement).checked)}
                />
              {:else}
                {e.allowed ? 'yes' : 'no'}
              {/if}
            </td>
            <td>
              {#if e.allowed && e.domain === 'light'}
                <div class="row">
                  <button class="small" disabled={busy === e.entity_id} onclick={() => command(e, 'turn_on')}>On</button>
                  <button class="small ghost" disabled={busy === e.entity_id} onclick={() => command(e, 'turn_off')}>Off</button>
                  <input
                    type="range" min="1" max="100" style="width:6rem"
                    value={brightness[e.entity_id] ?? 70}
                    oninput={(ev) => (brightness[e.entity_id] = +(ev.target as HTMLInputElement).value)}
                    onchange={() => command(e, 'turn_on')}
                    title="brightness %"
                  />
                </div>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
    <p class="dim small" style="margin:.6rem 0 0">
      Commands are policy-checked: imported + allowlisted + light domain only.
      State shown comes from the Home Assistant subscription, not the model.
    </p>
  </div>
{/if}

<h2 style="margin-top:2rem">Voice satellites</h2>
<div class="panel">
  <p class="dim small" style="margin-top:0">
    Pair a Windows PC running <code>adrian-satellite</code>. Generate a code, then on
    that PC run:
    <code>adrian-satellite pair --server {location.origin} --code &lt;code&gt;</code>
  </p>
  {#if isOwner}
    <div class="row">
      <button class="small" onclick={mintCode}>Generate pairing code</button>
      {#if pairCode}
        <span class="chip ok" style="font-size:1.2rem; letter-spacing:.15em">{pairCode}</span>
        <span class="dim small">expires {new Date(pairExpires * 1000).toLocaleTimeString()}</span>
      {/if}
    </div>
  {/if}
  {#if satellites.length === 0}
    <p class="dim small">No satellites paired yet.</p>
  {:else}
    <table style="margin-top:.6rem">
      <thead><tr><th>Name</th><th>Paired</th><th>Last seen</th><th>Status</th><th></th></tr></thead>
      <tbody>
        {#each satellites as s (s.id)}
          <tr>
            <td>{s.name}</td>
            <td class="small">{new Date(s.paired_at * 1000).toLocaleString()}</td>
            <td class="small">{s.last_seen ? new Date(s.last_seen * 1000).toLocaleString() : 'never'}</td>
            <td><span class="chip {s.revoked ? 'err' : 'ok'}">{s.revoked ? 'revoked' : 'active'}</span></td>
            <td>
              {#if isOwner && !s.revoked}
                <button class="small ghost" onclick={() => revokeSat(s.id)}>Revoke</button>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</div>
