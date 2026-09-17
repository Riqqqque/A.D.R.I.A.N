<script lang="ts">
  import { del, get, post } from '../api';
  import { audioEndpointId, audioEndpointStored, disableBrowserAudio, enableBrowserAudio } from '../audio';
  import type { Endpoint } from '../types';

  let endpoints = $state<Endpoint[]>([]);
  let error = $state('');
  let result = $state('');
  let name = $state('');
  let entityId = $state('');
  let browserOn = $state(audioEndpointStored());
  let testBusy = $state('');

  async function load() {
    try {
      const res = await get<{ endpoints: Endpoint[] }>('/api/endpoints');
      endpoints = res.endpoints;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }
  load();

  async function addHa() {
    error = '';
    try {
      await post('/api/endpoints', { name, kind: 'ha_media_player', entity_id: entityId });
      name = '';
      entityId = '';
      await load();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function toggleBrowser() {
    error = '';
    if (browserOn) {
      disableBrowserAudio();
      browserOn = false;
    } else {
      try {
        await enableBrowserAudio(`Browser — ${navigator.userAgent.includes('Windows') ? 'this PC' : 'this device'}`);
        browserOn = true;
        await load();
      } catch (e) {
        error = e instanceof Error ? e.message : String(e);
      }
    }
  }

  async function test(e: Endpoint) {
    testBusy = e.id;
    result = '';
    error = '';
    try {
      const r = await post<{ ok: boolean; detail: string }>(`/api/endpoints/${e.id}/test`, {});
      result = r.detail;
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      testBusy = '';
    }
  }

  async function remove(e: Endpoint) {
    if (!confirm(`Remove endpoint "${e.name}"?`)) return;
    try {
      await del(`/api/endpoints/${e.id}`);
      if (e.id === audioEndpointId()) disableBrowserAudio();
      browserOn = audioEndpointStored();
      await load();
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    }
  }
</script>

<h2>Audio endpoints</h2>
{#if error}<div class="banner">{error}</div>{/if}
{#if result}<div class="notice ok">{result}</div>{/if}

<div class="panel">
  <h2>This browser</h2>
  <p class="dim small">
    Registers this tab as a speaker for alarms. The tab must stay open and logged in.
    Click to enable — the click unlocks audio playback.
  </p>
  <button onclick={toggleBrowser} class={browserOn ? 'danger' : ''}>
    {browserOn ? 'Disable this browser as a speaker' : 'Use this browser as a speaker'}
  </button>
</div>

<div class="panel">
  <h2>Add a Home Assistant speaker</h2>
  <div class="row" style="align-items:end">
    <div class="grow"><label>Name</label><input style="width:100%" bind:value={name} placeholder="Kitchen speaker" /></div>
    <div class="grow"><label>Media player entity</label><input style="width:100%" bind:value={entityId} placeholder="media_player.kitchen" /></div>
    <button onclick={addHa} disabled={!name.trim() || !entityId.trim()}>Add</button>
  </div>
  <p class="dim small">Requires Home Assistant connected and a reachable <code>public_url</code> (set under System) if speakers can't reach this machine's hostname.</p>
</div>

<div class="panel">
  {#if endpoints.length === 0}<p class="dim">No endpoints yet.</p>{/if}
  <table>
    <tbody>
      {#each endpoints as e (e.id)}
        <tr>
          <td><strong>{e.name}</strong><br /><span class="dim small">{e.kind}{e.config.entity_id ? ` · ${e.config.entity_id}` : ''}</span></td>
          <td><span class="chip {e.enabled ? 'ok' : ''}">{e.enabled ? 'enabled' : 'disabled'}</span></td>
          <td style="text-align:right">
            <button class="small ghost" disabled={testBusy === e.id} onclick={() => test(e)}>{testBusy === e.id ? 'Testing…' : 'Test'}</button>
            <button class="small danger" onclick={() => remove(e)}>Remove</button>
          </td>
        </tr>
      {/each}
    </tbody>
  </table>
</div>
