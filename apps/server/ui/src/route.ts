// The page's address as path segments under `/ui`, such as
// `/ui/networks/ethereum/sepolia/blocks/12047224`: a reload or a shared link
// lands where it was. The server and the dev server answer every such path
// with the page.
import { useEffect, useState } from 'react';

const BASE = '/ui';

/** The segments of the current address, decoded. */
export function segments(): string[] {
  const path = window.location.pathname;
  const rest = path.startsWith(BASE) ? path.slice(BASE.length) : path;
  return rest.split('/').filter(Boolean).map(decodeURIComponent);
}

/** The address of `parts`, for a link. */
export function href(parts: string[]): string {
  return `${BASE}/${parts.map(encodeURIComponent).join('/')}`;
}

/** Goes to `parts`: a new history entry, or the current one replaced. */
export function go(parts: string[], replace = false): void {
  const url = href(parts);
  if (url === window.location.pathname) return;
  if (replace) window.history.replaceState(null, '', url);
  else window.history.pushState(null, '', url);
  window.dispatchEvent(new PopStateEvent('popstate'));
}

/** The current segments, following every move. */
export function useSegments(): string[] {
  const [current, setCurrent] = useState(segments);
  useEffect(() => {
    const follow = () => setCurrent(segments());
    window.addEventListener('popstate', follow);
    return () => window.removeEventListener('popstate', follow);
  }, []);
  return current;
}
