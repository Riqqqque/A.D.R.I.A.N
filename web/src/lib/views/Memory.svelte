<script lang="ts">
  import { del, get, post, put } from '../api';
  import { session } from '../session.svelte';
  import type { Fact } from '../types';

  let facts = $state<Fact[]>([]);
  let query = $state('');
  let newText = $state('');
  let newScope = $state('private');
  let editing = $state<number | null>(null);
  let editText = $state('');
  let error = $state('');
  const isOwner = $derived(session.user?.role !== 'guest');

  async function load() {
    try {
      const res = await get<{ facts: Fact[] }>(`/api/memory${query ? `?q=${encodeURIComponent(query)}` : ''}`);
      facts = res.facts;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }
  load();

  async function add() {
    error = '';
    try {
      await post('/api/memory', { text: newText, scope: newScope });
      newText = '';
      await load();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function save(f: Fact) {
    try {
      await put(`/api/memory/${f.id}`, { text: editText });
      editing = null;
      await load();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function remove(f: Fact) {
    if (!confirm(`Delete this fact? "${f.text.slice(0, 60)}"`)) return;
    try {
      await del(`/api/memory/${f.id}`);
      await load();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  function fmt(ts: number) {
    return new Date(ts * 1000).toLocaleString();
  }
</script>

<h2>Memory</h2>
{#if error}<div class="banner">{error}</div>{/if}
<div class="panel">
  <div class="row">
    <input class="grow" placeholder="Search saved facts…" bind:value={query} oninput={() => load()} />
  </div>
  <div class="row" style="margin-top:.6rem">
    <input class="grow" placeholder="Remember something, e.g. “the wifi password is on the fridge”" bind:value={newText} />
    <select bind:value={newScope} disabled={!isOwner}>
      <option value="private">private</option>
      <option value="household">household</option>
    </select>
    <button onclick={add} disabled={!newText.trim()}>Remember</button>
  </div>
</div>

<div class="panel">
  {#if facts.length === 0}
    <p class="dim">Nothing saved{query ? ' for this search' : ''} yet.</p>
  {/if}
  {#each facts as f (f.id)}
    <div style="padding:.5rem 0; border-bottom:1px solid var(--border)">
      {#if editing === f.id}
        <div class="row">
          <input class="grow" bind:value={editText} />
          <button class="small" onclick={() => save(f)}>Save</button>
          <button class="small ghost" onclick={() => (editing = null)}>Cancel</button>
        </div>
      {:else}
        <div class="row">
          <span class="grow">{f.text}</span>
          <span class="chip">{f.scope}</span>
          <button class="small ghost" onclick={() => { editing = f.id; editText = f.text; }}>Edit</button>
          <button class="small danger" onclick={() => remove(f)}>Delete</button>
        </div>
        <div class="small dim" style="margin-top:.15rem">
          by {f.owner} · {f.source} · {fmt(f.updated_at)} · v{f.version}
        </div>
      {/if}
    </div>
  {/each}
</div>
