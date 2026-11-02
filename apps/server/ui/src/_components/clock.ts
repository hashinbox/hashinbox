// One shared second tick for every live time on the page, so a table of
// countdowns runs on one timer, not one per row.
import { useSyncExternalStore } from 'react';

const listeners = new Set<() => void>();
let now = Date.now();
let timer: number | undefined;

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  timer ??= window.setInterval(() => {
    now = Date.now();
    for (const l of listeners) l();
  }, 1000);
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0 && timer !== undefined) {
      window.clearInterval(timer);
      timer = undefined;
    }
  };
}

/** The current time, Unix ms, updated every second. */
export function useNow(): number {
  return useSyncExternalStore(subscribe, () => now);
}

/** `ms` (a span of time) as `45s`, `3m 05s`, `2h 10m` or `4d 6h`. */
export function span(ms: number): string {
  const s = Math.floor(Math.abs(ms) / 1000);
  if (s < 60) return `${String(s)}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${String(m)}m ${String(s % 60).padStart(2, '0')}s`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${String(h)}h ${String(m % 60).padStart(2, '0')}m`;
  return `${String(Math.floor(h / 24))}d ${String(h % 24)}h`;
}
