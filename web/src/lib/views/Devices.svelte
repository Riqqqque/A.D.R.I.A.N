<script lang="ts">
  import { get, post } from '../api';
  import { session } from '../session.svelte';
  import type { Entity } from '../types';

  let entities = $state<Entity[]>([]);
  let error = $state('');
  let busy = $state('');
  let brightness = $state<Record<string, number>>({});
  const isOwner = $derived(session.user?.role === 'owner');

  async function load() {
    try {
      const res = await get<{ entities: Entity[] }>('/api/devices/entities');
      entities = res.entities;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }
  load();

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
