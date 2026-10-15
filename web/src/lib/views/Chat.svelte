<script lang="ts">
  import { get, sse } from '../api';
  import { session } from '../session.svelte';
  import { onMount } from 'svelte';

  interface ToolEvt {
    name: string;
    status: string;
    detail?: string;
  }
  interface Msg {
    role: 'user' | 'assistant';
    text: string;
    tools?: ToolEvt[];
  }
  interface Convo {
    id: string;
    title: string;
    created_at: number;
    last_message: string | null;
  }

  let messages = $state<Msg[]>([]);
  let convos = $state<Convo[]>([]);
  let input = $state('');
  let busy = $state(false);
  let error = $state('');
  let convId = $state<string | null>(null);
  let scrollEl: HTMLDivElement;

  $effect(() => {
    void messages.length;
    void messages[messages.length - 1]?.text;
    if (scrollEl) scrollEl.scrollTop = scrollEl.scrollHeight;
  });

  onMount(loadConvos);

  async function loadConvos() {
    try {
      const res = await get<{ conversations: Convo[] }>('/api/chat/conversations');
      convos = res.conversations;
    } catch {
      /* history is best-effort */
    }
  }

  async function openConvo(id: string | null) {
    if (!id) {
      convId = null;
      messages = [];
      return;
    }
    try {
      const res = await get<{ messages: { role: string; content: string }[] }>(
        `/api/chat/conversations/${id}`
      );
      convId = id;
      messages = res.messages
        .filter((m) => m.role === 'user' || m.role === 'assistant')
        .map((m) => ({ role: m.role as 'user' | 'assistant', text: m.content }));
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function send() {
    const text = input.trim();
    if (!text || busy) return;
    input = '';
    error = '';
    busy = true;
    messages.push({ role: 'user', text });
    messages.push({ role: 'assistant', text: '', tools: [] });
    const mine = messages[messages.length - 1];
    try {
      for await (const f of sse('/api/chat', { message: text, conversation_id: convId })) {
        if (f.event === 'delta') {
          mine.text += String(f.data.text ?? '');
        } else if (f.event === 'tool') {
          mine.tools = [
            ...(mine.tools ?? []),
            {
              name: String(f.data.name ?? 'tool'),
              status: String(f.data.status ?? ''),
              detail: f.data.detail ? String(f.data.detail) : undefined
            }
          ];
        } else if (f.event === 'notice') {
          mine.text += `\n\n(${String(f.data.message ?? '')})`;
        } else if (f.event === 'error') {
          mine.text += `\n\n${String(f.data.message ?? 'error')}`;
        } else if (f.event === 'done') {
          convId = (f.data.conversation_id as string) ?? convId;
        }
      }
      void loadConvos();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
      mine.text += `\n\n${error}`;
    } finally {
      busy = false;
    }
  }
</script>

{#if !session.flags?.provider_configured}
  <div class="banner">
    No AI provider is configured — chat can't answer yet.
    Add one under <strong>Integrations</strong>.
  </div>
{/if}

<div class="row" style="margin-bottom:.6rem">
  <select
    value={convId ?? ''}
    onchange={(e) => openConvo((e.target as HTMLSelectElement).value || null)}
  >
    <option value="">New conversation</option>
    {#each convos as c}
      <option value={c.id}>{c.title || 'Untitled'} — {new Date(c.created_at * 1000).toLocaleDateString()}</option>
    {/each}
  </select>
  <button class="ghost small" onclick={() => openConvo(null)}>New chat</button>
</div>

<div class="panel" style="display:flex; flex-direction:column; height: calc(100vh - 14.5rem);">
  <div bind:this={scrollEl} style="flex:1; overflow-y:auto; padding:.25rem;">
    {#if messages.length === 0}
      <p class="dim">Ask Adrian something, e.g. “turn off the living room light” or “remind me in 10 minutes to check the oven”.</p>
    {/if}
    {#each messages as m}
      <div class="msg {m.role}">
        {#if m.tools?.length}
          <div class="tools">
            {#each m.tools as t}
              <span class="chip {t.status === 'confirmed' || t.status === 'done' ? 'ok' : t.status === 'denied' || t.status === 'failed' ? 'err' : ''}">
                {t.name}: {t.status}{t.detail ? ` — ${t.detail}` : ''}
              </span>
            {/each}
          </div>
        {/if}
        <div class="bubble">{m.text || (m.role === 'assistant' && busy && messages[messages.length-1] === m ? '…' : '')}</div>
      </div>
    {/each}
  </div>
  <form class="row" style="margin-top:.6rem" onsubmit={(e) => { e.preventDefault(); send(); }}>
    <input class="grow" bind:value={input} placeholder="Message Adrian…" disabled={busy} />
    <button disabled={busy || !input.trim()}>Send</button>
  </form>
  {#if error}<p class="err small">{error}</p>{/if}
</div>

<style>
  .msg { margin: .4rem 0; }
  .msg.user .bubble {
    background: var(--accent-2);
    color: #fff;
    margin-left: auto;
    max-width: 75%;
  }
  .msg.assistant .bubble {
    background: var(--panel-2);
    margin-right: auto;
    max-width: 85%;
    white-space: pre-wrap;
  }
  .bubble { border-radius: 10px; padding: .55rem .8rem; width: fit-content; }
  .tools { display: flex; gap: .3rem; flex-wrap: wrap; margin-bottom: .3rem; }
</style>
