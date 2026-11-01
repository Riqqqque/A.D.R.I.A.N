// Browser speaker behaviour. web/ has no test runner, so this uses node:test
// and the TypeScript compiler already in devDependencies:
//   node --test web/tests/audio.test.mjs
// Each test loads a fresh copy of audio.ts with the browser APIs stubbed.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';

const source = (name) => readFileSync(fileURLToPath(new URL(`../src/lib/${name}`, import.meta.url)), 'utf8');
const transpile = (code) =>
  ts.transpileModule(code, {
    compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 }
  }).outputText;
const dataUrl = (js) => `data:text/javascript,${encodeURIComponent(js)}`;

let loads = 0;

/** Fresh audio module, a stubbed browser, and the requests it posted. */
async function load({ playFails = false, userActivation } = {}) {
  const posts = [];
  const players = [];
  const store = new Map();
  let channel = null;

  globalThis.localStorage = {
    getItem: (k) => store.get(k) ?? null,
    setItem: (k, v) => store.set(k, v),
    removeItem: (k) => store.delete(k)
  };
  Object.defineProperty(globalThis, 'navigator', { value: { userActivation }, configurable: true });
  globalThis.EventSource = class {
    constructor() {
      channel = this;
    }
    addEventListener(_kind, fn) {
      this.onCmd = fn;
    }
    close() {}
  };
  globalThis.Audio = class {
    paused = false;
    constructor(url) {
      this.url = url;
      players.push(this);
    }
    play() {
      return playFails ? Promise.reject(new Error('NotAllowedError')) : Promise.resolve();
    }
    pause() {
      this.paused = true;
    }
  };
  globalThis.fetch = async (path, init) => {
    posts.push({ path, body: JSON.parse(init.body) });
    return new Response('{"endpoint_id":"e1","channel_id":"c1"}', {
      headers: { 'content-type': 'application/json' }
    });
  };

  loads += 1;
  const api = dataUrl(`${transpile(source('api.ts'))}\n// ${loads}`);
  const audio = await import(
    dataUrl(transpile(source('audio.ts')).replaceAll("'./api'", () => JSON.stringify(api)))
  );
  await audio.enableBrowserAudio('Test');
  posts.length = 0; // the registration itself
  const send = async (cmd) => {
    channel.onCmd({ data: JSON.stringify(cmd) });
    await new Promise((resolve) => setTimeout(resolve));
  };
  return { audio, posts, players, send };
}

const play = (delivery_id) => ({ event: 'play', delivery_id, url: '/media/t', volume: 0.5 });
const stop = (delivery_id) => ({ event: 'stop', delivery_id });

test('a finished playback acks success', async () => {
  const { posts, players, send } = await load();
  await send(play('d1'));
  players[0].onended();
  await new Promise((resolve) => setTimeout(resolve));
  assert.deepEqual(posts, [
    { path: '/api/audio/ack', body: { delivery_id: 'd1', ok: true, result: 'played' } }
  ]);
});

test('blocked playback acks failure', async () => {
  const { posts, send } = await load({ playFails: true });
  await send(play('d1'));
  assert.equal(posts.length, 1);
  assert.equal(posts[0].body.ok, false);
  assert.match(posts[0].body.result, /NotAllowedError/);
});

test('a media error acks failure', async () => {
  const { posts, players, send } = await load();
  await send(play('d1'));
  players[0].onerror();
  await new Promise((resolve) => setTimeout(resolve));
  assert.equal(posts[0].body.ok, false);
});

test('stop silences the delivery this tab is playing', async () => {
  const { posts, players, send } = await load();
  await send(play('d1'));
  await send(stop('d1'));
  assert.equal(players[0].paused, true);
  assert.deepEqual(posts.map((p) => p.body), [
    { delivery_id: 'd1', ok: true, result: 'stopped by user' }
  ]);
});

test("stop for another delivery leaves this tab's playback alone", async () => {
  const { posts, players, send } = await load();
  await send(play('d1'));
  await send(stop('d2'));
  assert.equal(players[0].paused, false);
  assert.deepEqual(posts, []);
});

test('disabling tells the server which endpoint went away', async () => {
  const { audio, posts } = await load();
  assert.equal(await audio.disableBrowserAudio(), true);
  assert.deepEqual(posts, [
    { path: '/api/audio/unregister', body: { endpoint_id: 'e1', channel_id: 'c1' } }
  ]);
  assert.equal(audio.audioEndpointId(), '');
  assert.equal(audio.audioEndpointStored(), false);
});

test('a failed unregister still turns the speaker off here', async () => {
  const { audio } = await load();
  globalThis.fetch = async () => {
    throw new Error('offline');
  };
  assert.equal(await audio.disableBrowserAudio(), false);
  assert.equal(audio.audioEndpointStored(), false);
});

test('a resumed tab nobody has touched needs a click before it can sound', async () => {
  const { audio } = await load({ userActivation: { hasBeenActive: false } });
  assert.equal(audio.audioNeedsGesture(), true);
});

test('a tab that has been clicked plays freely', async () => {
  const { audio } = await load({ userActivation: { hasBeenActive: true } });
  assert.equal(audio.audioNeedsGesture(), false);
});

test('a browser without userActivation gets no false alarm', async () => {
  const { audio } = await load();
  assert.equal(audio.audioNeedsGesture(), false);
});
