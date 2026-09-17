import adapter from '@sveltejs/adapter-static';

export default {
  kit: {
    // Pure SPA: emit a single index.html shell; the Rust server falls back
    // to it for every non-API path.
    adapter: adapter({ pages: 'build', assets: 'build', fallback: 'index.html' }),
    prerender: { entries: [] }
  }
};
