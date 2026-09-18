<script lang="ts">
  import { del, get, post } from '../api';
  import type { Alarm, Endpoint } from '../types';

  let alarms = $state<Alarm[]>([]);
  let endpoints = $state<Endpoint[]>([]);
  let label = $state('');
  let mode = $state<'clock' | 'timer'>('timer');
  let atTime = $state('');
  let atDate = $state('');
  let inMinutes = $state<number | ''>(10);
  let recurrence = $state('none');
  let endpointId = $state('');
  let volume = $state(70);
  let error = $state('');
  let notice = $state('');

  async function load() {
    try {
      const [a, e] = await Promise.all([
        get<{ alarms: Alarm[] }>('/api/alarms'),
        get<{ endpoints: Endpoint[] }>('/api/endpoints')
      ]);
      alarms = a.alarms;
      endpoints = e.endpoints.filter((x) => x.enabled);
      if (!endpointId && endpoints.length) endpointId = endpoints[0].id;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }
  load();

  async function create() {
    error = '';
    notice = '';
    const body: Record<string, unknown> = {
      label,
      recurrence,
      endpoint_id: endpointId || undefined,
      volume: volume / 100
    };
    if (mode === 'timer') {
      if (!inMinutes || inMinutes < 1) {
        error = 'timer needs at least 1 minute';
        return;
      }
      body.in_minutes = inMinutes;
    } else {
      if (!atTime) {
        error = 'pick a time';
        return;
      }
      body.at_local = atDate ? `${atDate} ${atTime}` : atTime;
    }
    try {
      const res = await post<{ alarm: { resolved_local?: string; notes?: string[] } }>('/api/alarms', body);
      notice = `Scheduled for ${res.alarm.resolved_local}${res.alarm.notes?.length ? ` (${res.alarm.notes.join('; ')})` : ''}`;
      label = '';
      await load();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function act(a: Alarm, what: 'snooze' | 'stop' | 'delete') {
    try {
      if (what === 'delete') await del(`/api/alarms/${a.id}`);
      else await post(`/api/alarms/${a.id}/${what}`, {});
      await load();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  function due(a: Alarm) {
    return a.next_due_utc ? new Date(a.next_due_utc * 1000).toLocaleString() : '—';
  }
</script>

<h2>Alarms & timers</h2>
{#if error}<div class="banner">{error}</div>{/if}
{#if notice}<div class="notice ok">{notice}</div>{/if}

<div class="panel">
  <div class="row">
    <button class="small {mode === 'timer' ? '' : 'ghost'}" onclick={() => (mode = 'timer')}>Timer</button>
    <button class="small {mode === 'clock' ? '' : 'ghost'}" onclick={() => (mode = 'clock')}>Alarm clock</button>
  </div>
  <div class="row" style="margin-top:.6rem; align-items:end">
    <label class="field grow">Label
      <input bind:value={label} placeholder="e.g. take out laundry" />
    </label>
    {#if mode === 'timer'}
      <label class="field" style="width:6rem">In minutes
        <input type="number" min="1" max="43200" bind:value={inMinutes} />
      </label>
    {:else}
      <label class="field">Time<input type="time" bind:value={atTime} /></label>
      <label class="field">Date (optional)<input type="date" bind:value={atDate} /></label>
      <label class="field">Repeat
        <select bind:value={recurrence}>
          <option value="none">once</option><option value="daily">daily</option><option value="weekdays">weekdays</option>
        </select>
      </label>
    {/if}
    <label class="field">Speaker
      <select bind:value={endpointId}>
        {#if endpoints.length === 0}<option value="">— none configured —</option>{/if}
        {#each endpoints as e}<option value={e.id}>{e.name}</option>{/each}
      </select>
    </label>
    <label class="field">Volume {volume}%<input type="range" min="1" max="100" bind:value={volume} /></label>
    <div><button onclick={create}>Schedule</button></div>
  </div>
  {#if endpoints.length === 0}
    <p class="warn small">No audio endpoint configured — alarms can't make sound. Add one under <strong>Audio</strong>.</p>
  {/if}
</div>

<div class="panel">
  {#if alarms.length === 0}<p class="dim">No alarms yet.</p>{/if}
  {#each alarms as a (a.id)}
    <div class="row" style="padding:.5rem 0; border-bottom:1px solid var(--border)">
      <span class="chip {a.status === 'ringing' ? 'err' : a.status === 'scheduled' ? 'ok' : ''}">{a.status}</span>
      <span class="grow"><strong>{a.label || a.kind}</strong> <span class="dim">· {a.kind} · due {due(a)}{a.recurrence !== 'none' ? ` · repeats ${a.recurrence}` : ''} · {a.endpoint_name ?? 'no endpoint'}</span></span>
      {#if a.last_delivery_status === 'failed'}
        <span class="err small" title={a.last_delivery_detail}>delivery failed</span>
      {/if}
      {#if a.status === 'ringing'}
        <button class="small" onclick={() => act(a, 'snooze')}>Snooze {a.snooze_minutes}m</button>
        <button class="small danger" onclick={() => act(a, 'stop')}>Stop</button>
      {:else if a.status === 'scheduled'}
        <button class="small ghost" onclick={() => act(a, 'delete')}>Cancel</button>
      {/if}
    </div>
  {/each}
</div>
