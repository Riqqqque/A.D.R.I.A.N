<script lang="ts">
  import { del, get, post, put } from '../api';
  import type { Budget, ModelPrice, Provider, UsageSummary } from '../types';

  let summary = $state<UsageSummary | null>(null);
  let prices = $state<ModelPrice[]>([]);
  let providers = $state<Provider[]>([]);
  let error = $state('');
  let notice = $state('');

  // New budget form
  let bScope = $state<'global' | 'provider' | 'module'>('global');
  let bScopeId = $state('');
  let bLimit = $state(5);
  let bSoft = $state(80);

  // Price form
  let pProvider = $state('');
  let pModel = $state('');
  let pIn = $state(0);
  let pCached = $state<number | null>(null);
  let pOut = $state(0);
  let pAudio = $state(0);

  const modules = ['web', 'satellite', 'discord'];
  const cloudProviders = $derived(providers.filter((p) => !p.local));

  function usd(micros: number) {
    const d = micros / 1_000_000;
    return d !== 0 && Math.abs(d) < 1 ? `$${d.toFixed(4)}` : `$${d.toFixed(2)}`;
  }
  const perM = (micros: number | null) => (micros === null ? '—' : `$${(micros / 1_000_000).toFixed(2)}`);
  const k = (n: number) =>
    n >= 1_000_000 ? `${(n / 1_000_000).toFixed(2)}M` : n >= 10_000 ? `${(n / 1000).toFixed(1)}k` : String(n);

  function scopeLabel(b: Budget) {
    if (b.scope === 'global') return 'Household';
    if (b.scope === 'module') return `Module: ${b.scope_id}`;
    const p = providers.find((x) => x.id === b.scope_id);
    return `Provider: ${p?.label ?? b.scope_id}`;
  }

  async function load() {
    error = '';
    try {
      const [s, pr, pv] = await Promise.all([
        get<UsageSummary>('/api/usage'),
        get<{ prices: ModelPrice[] }>('/api/usage/prices'),
        get<{ providers: Provider[] }>('/api/integrations/providers')
      ]);
      summary = s;
      prices = pr.prices;
      providers = pv.providers;
      if (!pProvider && cloudProviders.length) pickProvider(cloudProviders[0].id);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }
  load();

  function pickProvider(id: string) {
    pProvider = id;
    pModel = providers.find((p) => p.id === id)?.model ?? '';
    const existing = prices.find((x) => x.provider_id === id && x.model === pModel);
    pIn = existing ? existing.input_per_mtok_micros / 1e6 : 0;
    pCached = existing?.cached_per_mtok_micros != null ? existing.cached_per_mtok_micros / 1e6 : null;
    pOut = existing ? existing.output_per_mtok_micros / 1e6 : 0;
    pAudio = existing ? existing.audio_per_minute_micros / 1e6 : 0;
  }

  async function run(f: () => Promise<unknown>, ok: string) {
    error = '';
    notice = '';
    try {
      await f();
      notice = ok;
      await load();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  const saveBudget = () =>
    run(
      () =>
        post('/api/usage/budgets', {
          scope: bScope,
          scope_id: bScope === 'global' ? '' : bScopeId,
          monthly_limit: Number(bLimit),
          soft_pct: Number(bSoft)
        }),
      'Budget saved.'
    );
  const removeBudget = (id: string) => run(() => del(`/api/usage/budgets/${id}`), 'Budget removed.');
  const savePrice = () =>
    run(
      () =>
        put('/api/usage/prices', {
          provider_id: pProvider,
          model: pModel,
          input_per_mtok: Number(pIn),
          cached_per_mtok: pCached === null || String(pCached) === '' ? null : Number(pCached),
          output_per_mtok: Number(pOut),
          audio_per_minute: Number(pAudio)
        }),
      'Price saved.'
    );
  const removePrice = (p: ModelPrice) =>
    run(
      () =>
        del(
          `/api/usage/prices?provider_id=${encodeURIComponent(p.provider_id)}&model=${encodeURIComponent(p.model)}`
        ),
      'Price removed.'
    );
</script>

<div class="panel">
  <h2>Usage and budgets</h2>
  {#if error}<div class="banner">{error}</div>{/if}
  {#if notice}<div class="notice ok">{notice}</div>{/if}

  {#if summary}
    <div class="row">
      <span class="chip">{summary.month}</span>
      <span class="chip ok">spent {usd(summary.total_cost_micros)}</span>
    </div>
    {#each summary.unpriced as u}
      <p class="warn small" style="margin:.3rem 0">· {u} has no price yet, so budgets can't bound it. Add one below.</p>
    {/each}

    {#if summary.rows.length === 0}
      <p class="dim">No provider calls this month.</p>
    {:else}
      <table style="margin-top:.6rem">
        <thead>
          <tr><th>Provider</th><th>Model</th><th>Use</th><th>Calls</th><th>Tokens in / cached / out</th><th>Audio</th><th>Cost</th></tr>
        </thead>
        <tbody>
          {#each summary.rows as r}
            <tr>
              <td>{r.provider}{#if r.local} <span class="chip ok">local</span>{/if}</td>
              <td class="small"><code>{r.model}</code></td>
              <td class="small">{r.capability} · {r.module}</td>
              <td>{r.calls}</td>
              <td class="small">{k(r.input_tokens)} / {k(r.cached_tokens)} / {k(r.output_tokens)}{#if r.estimated_calls} <span class="chip warn" title="provider reported no usage; counts estimated">est.</span>{/if}</td>
              <td class="small">{r.audio_seconds ? `${(r.audio_seconds / 60).toFixed(1)} min` : '—'}</td>
              <td>{usd(r.cost_micros)}{#if r.unpriced_calls} <span class="chip warn">unpriced</span>{/if}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}

    <h3 style="margin-top:1rem">Monthly budgets</h3>
    <p class="dim small">Paid calls stop once a budget is used up. Local models are free and never blocked. You get one warning per month at the soft limit.</p>
    {#if summary.budgets.length === 0}<p class="dim">No budgets set.</p>{/if}
    <table>
      <tbody>
        {#each summary.budgets as b (b.id)}
          {@const pct = b.monthly_limit_micros ? Math.min(100, (b.spent_micros / b.monthly_limit_micros) * 100) : 100}
          <tr>
            <td>{scopeLabel(b)}</td>
            <td class="small">{usd(b.spent_micros)} of {usd(b.monthly_limit_micros)}</td>
            <td style="width:30%">
              <div class="meter" role="progressbar" aria-valuenow={Math.round(pct)} aria-valuemin={0} aria-valuemax={100} aria-label="budget used">
                <div class="fill {pct >= 100 ? 'err' : pct >= b.soft_pct ? 'warn' : 'ok'}" style="width:{pct}%"></div>
              </div>
            </td>
            <td class="small dim">warn at {b.soft_pct}%</td>
            <td><button class="danger" onclick={() => removeBudget(b.id)}>Remove</button></td>
          </tr>
        {/each}
      </tbody>
    </table>
    <div class="row" style="margin-top:.5rem; align-items:end">
      <label class="field">Scope
        <select bind:value={bScope} onchange={() => (bScopeId = bScope === 'module' ? 'web' : (providers[0]?.id ?? ''))}>
          <option value="global">household</option><option value="provider">provider</option><option value="module">module</option>
        </select>
      </label>
      {#if bScope === 'provider'}
        <label class="field">Provider
          <select bind:value={bScopeId}>{#each providers as p}<option value={p.id}>{p.label}</option>{/each}</select>
        </label>
      {:else if bScope === 'module'}
        <label class="field">Module
          <select bind:value={bScopeId}>{#each modules as m}<option value={m}>{m}</option>{/each}</select>
        </label>
      {/if}
      <label class="field" style="width:7rem">Limit (USD/month)<input type="number" min="0" step="0.5" bind:value={bLimit} /></label>
      <label class="field" style="width:6rem">Warn at %<input type="number" min="1" max="100" bind:value={bSoft} /></label>
      <button onclick={saveBudget}>Save budget</button>
    </div>

    <h3 style="margin-top:1rem">Prices</h3>
    <p class="dim small">Enter your provider's current prices. Nothing is built in, because prices change.</p>
    {#if prices.length}
      <table>
        <thead><tr><th>Provider</th><th>Model</th><th>In / 1M</th><th>Cached / 1M</th><th>Out / 1M</th><th>Audio / min</th><th></th></tr></thead>
        <tbody>
          {#each prices as p}
            <tr>
              <td>{providers.find((x) => x.id === p.provider_id)?.label ?? p.provider_id}</td>
              <td class="small"><code>{p.model}</code></td>
              <td>{perM(p.input_per_mtok_micros)}</td>
              <td>{perM(p.cached_per_mtok_micros)}</td>
              <td>{perM(p.output_per_mtok_micros)}</td>
              <td>{usd(p.audio_per_minute_micros)}</td>
              <td><button class="ghost" onclick={() => removePrice(p)}>Remove</button></td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
    {#if cloudProviders.length === 0}
      <p class="dim small">Only cloud providers need prices. You have none configured.</p>
    {:else}
      <div class="row" style="margin-top:.5rem; align-items:end">
        <label class="field">Provider
          <select value={pProvider} onchange={(e) => pickProvider((e.currentTarget as HTMLSelectElement).value)}>
            {#each cloudProviders as p}<option value={p.id}>{p.label}</option>{/each}
          </select>
        </label>
        <label class="field">Model<input bind:value={pModel} /></label>
        <label class="field" style="width:6rem">In $/1M<input type="number" min="0" step="0.01" bind:value={pIn} /></label>
        <label class="field" style="width:6rem">Cached $/1M<input type="number" min="0" step="0.01" bind:value={pCached} placeholder="= in" /></label>
        <label class="field" style="width:6rem">Out $/1M<input type="number" min="0" step="0.01" bind:value={pOut} /></label>
        <label class="field" style="width:6rem">Audio $/min<input type="number" min="0" step="0.001" bind:value={pAudio} /></label>
        <button onclick={savePrice}>Save price</button>
      </div>
    {/if}
  {/if}
</div>

<style>
  .meter { height: .5rem; border-radius: 999px; background: var(--border); overflow: hidden; }
  .fill { height: 100%; }
  .fill.ok { background: var(--ok); }
  .fill.warn { background: var(--warn); }
  .fill.err { background: var(--err); }
</style>
