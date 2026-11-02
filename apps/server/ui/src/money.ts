// Exact amounts. The API sends decimal strings in display units; nothing here
// ever goes through a float.
import type { Listing, Transaction, Wallet } from '@/api/client';
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

/** The wallet's first indexed asset: the asset this UI reads and sends. */
export function assetOf(wallet: Wallet): string {
  return wallet.assets[0] ?? 'native';
}

export function listingOf(wallet: Wallet, listings: Listing[]): Listing | undefined {
  return listings.find(
    (l) =>
      l.asset === assetOf(wallet) &&
      l.network.chain === wallet.network.chain &&
      l.network.name === wallet.network.name,
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

/** What one transaction did to one wallet in one asset. */
export interface Effect {
  credit: bigint;
  debit: bigint;
  net: bigint;
  /** The fee this wallet paid in the asset; 0 when none. */
  fee: bigint;
}

/** Folds a transaction's movements, all of this wallet in this asset. */
export function effect(tx: Transaction, decimals: number): Effect {
  let credit = 0n;
  let debit = 0n;
  let fee = 0n;
  for (const m of tx.movements) {
    const amount = atomic(m.amount, decimals);
    if (m.kind === 'received' || m.kind === 'minted') credit += amount;
    else if (m.kind === 'fee') fee += amount;
    else debit += amount;
  }
  return { credit, debit, net: credit - debit - fee, fee };
}

export { shorten } from '@/_components/trim';
