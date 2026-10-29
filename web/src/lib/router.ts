// History routing without hashes. The server answers every non-API path
// with index.html, so deep links like /m/<id> load the app directly.

export type Route =
  | { name: 'list' }
  | { name: 'meeting'; id: string; t: number | null }
  | { name: 'todo' }
  | { name: 'people' }
  | { name: 'settings' }
  | { name: 'ask' }
  | { name: 'not_found' };

/** Maps a pathname (and optional `?t=<ms>` search string) to a route. */
export function match(pathname: string, search = ''): Route {
  if (pathname === '/' || pathname === '') return { name: 'list' };
  if (pathname === '/todo') return { name: 'todo' };
  if (pathname === '/people') return { name: 'people' };
  if (pathname === '/settings') return { name: 'settings' };
  if (pathname === '/ask') return { name: 'ask' };
  const m = /^\/m\/([^/]+)\/?$/.exec(pathname);
  if (m) {
    let id: string;
    try {
      id = decodeURIComponent(m[1]);
    } catch {
      return { name: 'not_found' };
    }
    const t = Number(new URLSearchParams(search).get('t'));
    return { name: 'meeting', id, t: Number.isFinite(t) && t > 0 ? t : null };
  }
  return { name: 'not_found' };
}

/** Path of a meeting page, optionally starting playback at `t` ms. */
export function meetingPath(id: string, t?: number): string {
  const base = `/m/${encodeURIComponent(id)}`;
  return t !== undefined ? `${base}?t=${Math.floor(t)}` : base;
}

type Listener = (r: Route) => void;
const listeners = new Set<Listener>();

function emit() {
  const r = current();
  for (const l of listeners) l(r);
}

export function current(): Route {
  return match(location.pathname, location.search);
}

export function navigate(path: string, opts: { replace?: boolean } = {}): void {
  if (opts.replace) history.replaceState(null, '', path);
  else history.pushState(null, '', path);
  window.scrollTo(0, 0);
  emit();
}

/** Calls `fn` on every route change; returns an unsubscribe function. */
export function onRoute(fn: Listener): () => void {
  if (listeners.size === 0) window.addEventListener('popstate', emit);
  listeners.add(fn);
  return () => {
    listeners.delete(fn);
    if (listeners.size === 0) window.removeEventListener('popstate', emit);
  };
}

/**
 * Click handler for in-app `<a href>` links: plain left clicks route
 * in-page, modified clicks keep the browser's behaviour (new tab etc.).
 */
export function link(e: MouseEvent): void {
  if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) {
    return;
  }
  const a = (e.currentTarget as HTMLElement).closest('a');
  if (!a || a.target || a.origin !== location.origin) return;
  e.preventDefault();
  navigate(a.pathname + a.search);
}
