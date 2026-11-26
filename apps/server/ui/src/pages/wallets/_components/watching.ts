// What a wallet is watched in, from when, and how far it is indexed.
import { walletGet, walletIndexesCreate, type Wallet } from '@hashinbox/client';
import { api } from '@/api/client';

/** What to watch: the assets, and where indexing begins (its birthday). */
export interface Watching {
  assets: string[];
  /** `fresh`: from the chain's tip, for an address never used; else a native position. */
  start: 'fresh' | 'at';
  /** The position, when `start` is `at`. */
  from: string;
}

export const WATCH_NOTHING: Watching = { assets: [], start: 'fresh', from: '' };

/** Indexes wallet `id` in each asset of `watching` from its start; the wallet after. */
export async function watch(id: string, watching: Watching): Promise<Wallet> {
  const from = Number(watching.from);
  // No start: from the chain's tip.
  const at = watching.start === 'at' && Number.isInteger(from) && from >= 0;
  for (const asset of watching.assets)
    await walletIndexesCreate(api, { id, body: { asset, ...(at && { start: from }) } });
  return walletGet(api, { id });
}

/** How far one asset of a wallet is indexed. */
type Progress = Wallet['indexes'][string];

/** One index's state, compactly; `color` marks anything but plain indexing. */
export function progressOf(index: Progress): { text: string; color: string } {
  const shortened =
    index.birthday > index.requested_from
      ? ` · history from #${index.birthday} (asked #${index.requested_from})`
      : '';
  if (index.state === 'no_data') {
    return { text: `no data: ${index.reason ?? 'unknown'}${shortened}`, color: 'error.main' };
  }
  if (index.state === 'paused') {
    return { text: `paused${shortened}`, color: 'warning.main' };
  }
  const behind = index.behind_tip == null ? '' : ` · ${index.behind_tip} behind`;
  const final = index.finality_at == null ? '' : ` · final at #${index.finality_at}`;
  return { text: `indexing${behind}${final}${shortened}`, color: 'text.secondary' };
}

/** What stopped the wallet's indexing, `no data` over `paused`; null when nothing did. */
export function haltOf(wallet: Wallet): { text: string; color: string } | null {
  const states = Object.values(wallet.indexes).map((index) => index.state);
  if (states.includes('no_data')) return { text: 'no data', color: 'error.main' };
  if (states.includes('paused')) return { text: 'paused', color: 'warning.main' };
  return null;
}
