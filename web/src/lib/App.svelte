<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { post } from './api';
  import { session } from './session.svelte';
  import { audioEndpointStored, enableBrowserAudio } from './audio';
  import Chat from './views/Chat.svelte';
  import Devices from './views/Devices.svelte';
  import Memory from './views/Memory.svelte';
  import Alarms from './views/Alarms.svelte';
  import Endpoints from './views/Endpoints.svelte';
  import Integrations from './views/Integrations.svelte';
  import System from './views/System.svelte';

  type Tab = 'chat' | 'devices' | 'memory' | 'alarms' | 'audio' | 'integrations' | 'system';
  let tab = $state<Tab>('chat');
  let ringing = $state<{ id: string; label: string } | null>(null);
  let events: EventSource | null = null;

  const tabs: { id: Tab; label: string }[] = [
    { id: 'chat', label: 'Chat' },
    { id: 'devices', label: 'Devices' },
    { id: 'memory', label: 'Memory' },
    { id: 'alarms', label: 'Alarms' },
    { id: 'audio', label: 'Audio' },
    { id: 'integrations', label: 'Integrations' },
    { id: 'system', label: 'System' }
  ];

  onMount(() => {
    // Resume browser audio if this tab was previously registered.
    if (audioEndpointStored()) void enableBrowserAudio('Browser');
    events = new EventSource('/api/events');
    events.addEventListener('alarm', (e) => {
      try {
        const d = JSON.parse((e as MessageEvent).data) as { event: string; alarm_id?: string; label?: string };
        if (d.event === 'ringing' && d.alarm_id) {
          ringing = { id: d.alarm_id, label: d.label ?? 'Alarm' };
        } else if (ringing && d.alarm_id === ringing.id) {
          ringing = null;
        }
      } catch { /* ignore */ }
    });
  });
  onDestroy(() => events?.close());

  async function alarmAct(what: 'snooze' | 'stop') {
    if (!ringing) return;
    const id = ringing.id;
    ringing = null;
    try {
      await post(`/api/alarms/${id}/${what}`, {});
    } catch { /* shown on alarms tab */ }
  }

  async function logout() {
    try {
      await post('/api/auth/logout', {});
    } finally {
      location.reload();
    }
  }
</script>

<header>
  <strong>{session.settings.assistant_name || 'Adrian'}</strong>
  <nav>
    {#each tabs as t}
      <button class="tab" class:active={tab === t.id} onclick={() => (tab = t.id)}>{t.label}</button>
    {/each}
  </nav>
  <span class="dim small">{session.user?.username} · {session.user?.role}</span>
  <button class="ghost small" onclick={logout}>Sign out</button>
</header>

{#if ringing}
  <div class="ringing">
    <span class="grow"><strong>{ringing.label}</strong> is ringing</span>
    <button onclick={() => alarmAct('snooze')}>Snooze</button>
    <button class="danger" onclick={() => alarmAct('stop')}>Stop</button>
  </div>
{/if}

<main>
  {#if tab === 'chat'}<Chat />{/if}
  {#if tab === 'devices'}<Devices />{/if}
  {#if tab === 'memory'}<Memory />{/if}
  {#if tab === 'alarms'}<Alarms />{/if}
  {#if tab === 'audio'}<Endpoints />{/if}
  {#if tab === 'integrations'}<Integrations />{/if}
  {#if tab === 'system'}<System />{/if}
</main>

<style>
  header {
    display: flex;
    align-items: center;
    gap: 1rem;
    padding: .6rem 1rem;
    border-bottom: 1px solid var(--border);
    background: var(--panel);
    position: sticky;
    top: 0;
    z-index: 10;
  }
  nav { display: flex; gap: .2rem; flex: 1; }
  .tab {
    background: transparent;
    color: var(--dim);
    border: 0;
    padding: .35rem .7rem;
    border-radius: 7px;
  }
  .tab:hover { color: var(--text); }
  .tab.active { background: var(--panel-2); color: var(--text); }
  main { max-width: 60rem; margin: 1.2rem auto; padding: 0 1rem; }
  .ringing {
    display: flex;
    align-items: center;
    gap: .8rem;
    background: #4d2c2e;
    border-bottom: 1px solid var(--err);
    padding: .6rem 1rem;
    color: #ffd7d9;
  }
</style>
