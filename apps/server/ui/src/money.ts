// Exact amounts. The API sends decimal strings in display units; nothing here
// ever goes through a float.
import type { Listing, Network, Wallet } from '@hashinbox/client';
import { shorten } from '@/_components/trim';

/** Display text to atomic units, given the asset's decimals. */
export function atomic(text: string, decimals: number): bigint {
  const negative = text.startsWith('-');
  const [whole = '0', fraction = ''] = (negative ? text.slice(1) : text).split('.');
  const value = BigInt(whole + fraction.slice(0, decimals).padEnd(decimals, '0'));
  return negative ? -value : value;
}

const THIN = ' ';

/** Atomic units back to display text, grouped by thousands, trailing zeros trimmed. */
export function format(value: bigint, decimals: number): string {
  const negative = value < 0n;
  const digits = (negative ? -value : value).toString().padStart(decimals + 1, '0');
  const cut = digits.length - decimals;
  const whole = digits.slice(0, cut).replace(/\B(?=(\d{3})+(?!\d))/g, THIN);
  const fraction = digits.slice(cut).replace(/0+$/, '');
  return `${negative ? '−' : ''}${whole}${fraction ? `.${fraction}` : ''}`;
}

/** The asset ids the wallet is indexed in: native first, then tokens by id. */
export function assetsOf(wallet: Wallet): string[] {
  return Object.keys(wallet.indexes).sort(
    (a, b) => Number(b === 'native') - Number(a === 'native') || a.localeCompare(b),
  );
}

/** The wallet's first indexed asset (native when indexed): the asset this UI reads and sends. */
export function assetOf(wallet: Wallet): string {
  return assetsOf(wallet)[0] ?? 'native';
}

/** The wallet's chain and network as one object. */
export function networkOf(wallet: Wallet): Network {
  return { chain: wallet.chain, name: wallet.network };
}

export function listingOf(
  wallet: Wallet,
  listings: Listing[],
  asset = assetOf(wallet),
): Listing | undefined {
  return listings.find(
    (l) =>
      l.asset === asset && l.network.chain === wallet.chain && l.network.name === wallet.network,
  );
}

/** Every listed network as `chain/network`, with its native currency's ticker. */
export function networkChoices(listings: Listing[]): { value: string; ticker: string | null }[] {
  return listings
    .filter((l) => l.asset === 'native')
    .map((l) => ({ value: `${l.network.chain}/${l.network.name}`, ticker: l.metadata.ticker }))
    .sort((a, b) => a.value.localeCompare(b.value));
}

/** What `asset` of `chain/network` is called: its ticker, else its id shortened. */
export function assetName(
  listings: Listing[],
  chain: string,
  network: string,
  asset: string,
): string {
  const listing = listings.find(
    (l) => l.asset === asset && l.network.chain === chain && l.network.name === network,
  );
  return listing?.metadata.ticker ?? shorten(asset);
}

export function unitOf(wallet: Wallet, listings: Listing[]): string {
  return listingOf(wallet, listings)?.metadata.ticker ?? assetOf(wallet).toUpperCase();
}

/** Fractional digits views show of the wallet's asset; `null`: all. */
export function displayOf(wallet: Wallet, listings: Listing[]): number | null {
  return listingOf(wallet, listings)?.metadata.display_decimals ?? null;
}

export function decimalsOf(wallet: Wallet, listings: Listing[]): number {
  return listingOf(wallet, listings)?.metadata.decimals ?? 0;
}

export { shorten } from '@/_components/trim';

/** The tickers of every asset `wallet` is indexed in, native first; its unit when none. */
export function tickersOf(wallet: Wallet, listings: Listing[]): string[] {
  const assets = assetsOf(wallet);
  if (assets.length === 0) return [unitOf(wallet, listings)];
  return assets.map((asset) => assetName(listings, wallet.chain, wallet.network, asset));
}
