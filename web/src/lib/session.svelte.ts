import { get, setCsrf } from './api';
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
      this.error = e instanceof Error ? e.message : String(e);
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
