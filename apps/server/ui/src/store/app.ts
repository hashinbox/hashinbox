// What the whole app shares: the server's setup state and name (`GET
// /v1/version`), the assets wallets can be made in, and the toast. Pages
// read it with selectors (`useApp((s) => s.listings)`) and report with
// `notify` and `fail`.
import { create } from 'zustand';
import { ApiError, type Listing } from '@hashinbox/client';
import type { Message } from '@/_components/toast';

/**
 * Where setup stands (`GET /v1/version`): not asked yet, no user (fresh
 * install, until setup is done), or done.
 */
export type Root = 'unknown' | 'missing' | 'present';

interface AppState {
  /** The organization's name: the `instance` setting, `hashinbox` when unset. */
  name: string;
  root: Root;
  /** `GET /v1/assets`: what amounts are denominated in. */
  listings: Listing[];
  toast: Message | null;
}

export const useApp = create<AppState>()(() => ({
  name: 'hashinbox',
  root: 'unknown',
  listings: [],
  toast: null,
}));

/** One line at the bottom of the screen; `bad` in red. */
export function notify(text: string, bad = false): void {
  useApp.setState({ toast: { text, bad } });
}

/** `error` as a red toast; a 401 is the session's end, said once already. */
export function fail(error: unknown): void {
  const failure = ApiError.from(error);
  if (failure.status === 401) return;
  notify(failure.message, true);
}
