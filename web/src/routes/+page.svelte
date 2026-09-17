<script lang="ts">
  import { onMount } from 'svelte';
  import { session } from '../lib/session.svelte';
  import Setup from '../lib/Setup.svelte';
  import Login from '../lib/Login.svelte';
  import App from '../lib/App.svelte';

  onMount(() => session.boot());
</script>

{#if session.phase === 'loading'}
  <main style="display:grid;place-items:center;min-height:100vh">
    <p class="dim">Starting…</p>
  </main>
{:else if session.phase === 'setup'}
  <Setup />
{:else if session.phase === 'login'}
  <Login />
{:else}
  <App />
{/if}
