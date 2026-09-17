<script lang="ts">
  import { post } from './api';
  import { session } from './session.svelte';

  const timezones: string[] =
    'supportedValuesOf' in Intl
      ? (Intl.supportedValuesOf('timeZone') as string[])
      : ['UTC'];

  let token = $state('');
  let username = $state('');
  let displayName = $state('');
  let password = $state('');
  let confirm = $state('');
  let timezone = $state(Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC');
  let privacy = $state('hybrid');
  let busy = $state(false);
  let error = $state('');

  async function submit() {
    error = '';
    if (password !== confirm) {
      error = 'passwords do not match';
      return;
    }
    busy = true;
    try {
      await post('/api/setup/claim', {
        token: token.trim(),
        username: username.trim(),
        display_name: displayName.trim(),
        password,
        timezone,
        privacy_mode: privacy
      });
      await session.loadMe();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }
</script>

<main>
  <div class="panel" style="max-width: 34rem; margin: 4rem auto;">
    <h1>Set up Adrian</h1>
    <p class="dim">
      First-run claim. The setup token was printed to the server console and written to
      <code>SETUP_TOKEN.txt</code> in the data directory. It works once.
    </p>
    <form onsubmit={(e) => { e.preventDefault(); submit(); }}>
      <label>Setup token</label>
      <input class="grow" bind:value={token} required autocomplete="off" />
      <label>Username</label>
      <input class="grow" bind:value={username} required autocomplete="username" />
      <label>Display name (optional)</label>
      <input class="grow" bind:value={displayName} />
      <label>Password (min 8 chars)</label>
      <input class="grow" type="password" bind:value={password} required minlength="8" autocomplete="new-password" />
      <label>Confirm password</label>
      <input class="grow" type="password" bind:value={confirm} required autocomplete="new-password" />
      <label>Timezone</label>
      <select class="grow" bind:value={timezone}>
        {#each timezones as tz}<option value={tz}>{tz}</option>{/each}
      </select>
      <label>Privacy mode</label>
      <div class="row">
        <label class="row" style="margin:0">
          <input type="radio" bind:group={privacy} value="hybrid" /> hybrid — cloud providers allowed
        </label>
        <label class="row" style="margin:0">
          <input type="radio" bind:group={privacy} value="strict_local" /> strict-local — no cloud calls
        </label>
      </div>
      {#if error}<p class="err">{error}</p>{/if}
      <div style="margin-top: 1rem;">
        <button disabled={busy}>{busy ? 'Claiming…' : 'Create owner account'}</button>
      </div>
    </form>
  </div>
</main>

<style>
  input.grow, select.grow { width: 100%; }
</style>
