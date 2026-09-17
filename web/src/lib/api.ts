// Minimal API client. Session is a cookie; CSRF is required on mutations.

export class ApiError extends Error {
  code: string;
  constructor(code: string, message: string) {
    super(message);
    this.code = code;
  }
}

let csrf = '';
export function setCsrf(v: string) {
  csrf = v;
}

async function api<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {};
  if (body !== undefined) headers['content-type'] = 'application/json';
  if (method !== 'GET' && csrf) headers['x-adrian-csrf'] = csrf;
  const res = await fetch(path, {
    method,
    headers,
    credentials: 'same-origin',
    body: body === undefined ? undefined : JSON.stringify(body)
  });
  const ct = res.headers.get('content-type') ?? '';
  const data: unknown = ct.includes('json') ? await res.json() : await res.text();
  if (!res.ok) {
    const err = (data as { error?: { code?: string; message?: string } })?.error;
    throw new ApiError(err?.code ?? `http_${res.status}`, err?.message ?? String(data));
  }
  return data as T;
}

export const get = <T>(p: string) => api<T>('GET', p);
export const post = <T>(p: string, b?: unknown) => api<T>('POST', p, b ?? {});
export const put = <T>(p: string, b?: unknown) => api<T>('PUT', p, b ?? {});
export const del = <T>(p: string) => api<T>('DELETE', p);

export interface SseFrame {
  event: string;
  data: Record<string, unknown>;
}

/** Parse an SSE stream from a POST response (EventSource can't POST). */
export async function* sse(path: string, body: unknown): AsyncGenerator<SseFrame> {
  const res = await fetch(path, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'x-adrian-csrf': csrf },
    credentials: 'same-origin',
    body: JSON.stringify(body)
  });
  if (!res.ok || !res.body) {
    const data: unknown = await res.json().catch(() => ({}));
    const err = (data as { error?: { message?: string } })?.error;
    throw new ApiError(`http_${res.status}`, err?.message ?? `request failed (${res.status})`);
  }
  const reader = res.body.getReader();
  const dec = new TextDecoder();
  let buf = '';
  let ev = 'message';
  let dataLines: string[] = [];
  const flush = (): SseFrame | null => {
    if (dataLines.length === 0) {
      ev = 'message';
      return null;
    }
    const f: SseFrame = { event: ev, data: JSON.parse(dataLines.join('\n')) };
    ev = 'message';
    dataLines = [];
    return f;
  };
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buf += dec.decode(value, { stream: true });
    let idx: number;
    while ((idx = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, idx).replace(/\r$/, '');
      buf = buf.slice(idx + 1);
      if (line === '') {
        const f = flush();
        if (f) yield f;
      } else if (line.startsWith('event:')) {
        ev = line.slice(6).trim();
      } else if (line.startsWith('data:')) {
        dataLines.push(line.slice(5).trim());
      }
    }
  }
  const f = flush();
  if (f) yield f;
}
