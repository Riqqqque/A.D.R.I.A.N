import { ApiError, get, setCsrf } from './api';
import type { MeResponse, SetupFlags, User } from './types';

export type Phase = 'loading' | 'setup' | 'login' | 'app';

class Session {
  phase = $state<Phase>('loading');
  user = $state<User | null>(null);
  settings = $state<Record<string, string>>({});
  flags = $state<SetupFlags | null>(null);
  error = $state('');

  async boot() {
    this.error = '';
    try {
      const st = await get<{ initialized: boolean; setup_pending: boolean }>('/api/setup/status');
      if (!st.initialized) {
        this.phase = 'setup';
        return;
      }
      await this.loadMe();
    } catch (e) {
      // 401 just means "not signed in yet"; anything else is a real problem.
      const signedOut = e instanceof ApiError && (e.code === 'unauthorized' || e.code === 'http_401');
      this.error = signedOut ? '' : e instanceof Error ? e.message : String(e);
      this.phase = 'login';
    }
  }

  async loadMe() {
    const me = await get<MeResponse>('/api/auth/me');
    setCsrf(me.csrf);
    this.user = me.user;
    this.settings = me.settings;
    this.flags = me.setup;
    this.phase = 'app';
  }

  async refreshFlags() {
    try {
      const me = await get<MeResponse>('/api/auth/me');
      setCsrf(me.csrf);
      this.settings = me.settings;
      this.flags = me.setup;
    } catch {
      /* session may have expired */
    }
  }
}

export const session = new Session();
