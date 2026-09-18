<script lang="ts">
  import { post } from './api';
  import { session } from './session.svelte';

  let username = $state('');
  let password = $state('');
  let busy = $state(false);
  let error = $state('');

  async function submit() {
    error = '';
    busy = true;
    try {
      await post('/api/auth/login', { username: username.trim(), password });
      await session.loadMe();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }
</script>

<main>
  <div class="panel" style="max-width: 24rem; margin: 5rem auto;">
    <h1>Adrian</h1>
    {#if session.error}<p class="warn small">could not reach the server: {session.error}</p>{/if}
    <form onsubmit={(e) => { e.preventDefault(); submit(); }}>
      <label class="field">Username
        <input bind:value={username} required autocomplete="username" />
      </label>
      <label class="field">Password
        <input type="password" bind:value={password} required autocomplete="current-password" />
      </label>
      {#if error}<p class="err">{error}</p>{/if}
      <div style="margin-top: 1rem;">
        <button disabled={busy}>{busy ? 'Signing in…' : 'Sign in'}</button>
      </div>
    </form>
  </div>
</main>
