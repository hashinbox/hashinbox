// What a wallet is watched in, and from when.

/** What to watch: the assets, and where indexing begins (its birthday). */
export interface Watching {
  assets: string[];
  /** `fresh`: from the chain's tip, for an address never used; else a native position. */
  start: 'fresh' | 'at';
  /** The position, when `start` is `at`. */
  from: string;
}

export const WATCH_NOTHING: Watching = { assets: [], start: 'fresh', from: '' };

/** The request fields `watching` makes: assets and start. */
export function watchBody(watching: Watching): { assets: string[]; start?: 'fresh' | number } {
  const from = Number(watching.from);
  return {
    assets: watching.assets,
    ...(watching.start === 'at' && Number.isInteger(from) && from >= 0
      ? { start: from }
      : { start: 'fresh' as const }),
  };
}
