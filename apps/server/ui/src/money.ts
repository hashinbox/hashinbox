// Exact amounts. The API sends decimal strings in display units; nothing here
// ever goes through a float.
import type { Fee, Listing, Transaction, Wallet } from '@/api/client';

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

/** The wallet's first observed asset tag: the asset this UI reads and sends. */
export function assetOf(wallet: Wallet): string {
  const first = wallet.assets[0];
  return first ? (first.asset ?? first.asset_id) : '';
}

export function listingOf(wallet: Wallet, listings: Listing[]): Listing | undefined {
  return listings.find(
    (l) =>
      l.asset === assetOf(wallet) &&
      l.network.chain === wallet.network.chain &&
      l.network.name === wallet.network.name,
  );
}

export function unitOf(wallet: Wallet, listings: Listing[]): string {
  return listingOf(wallet, listings)?.metadata.ticker ?? assetOf(wallet).toUpperCase();
}

export function decimalsOf(wallet: Wallet, listings: Listing[]): number {
  return listingOf(wallet, listings)?.metadata.decimals ?? 0;
}

/** What one transaction did to one wallet. */
export interface Effect {
  credit: bigint;
  debit: bigint;
  net: bigint;
  counterparties: string[];
  /** The fee, when this wallet paid it. */
  fee: Fee | null;
}

/**
 * Folds a transaction's movements onto one address. A movement between two of
 * the address's own endpoints nets to zero, the same rule the server's balance
 * fold uses, so a list of these sums to the confirmed balance.
 */
export function effect(wallet: Wallet, tx: Transaction, decimals: number): Effect {
  const mine = wallet.address.toLowerCase();
  const own = (value: string | undefined) => value?.toLowerCase() === mine;
  let credit = 0n;
  let debit = 0n;
  const others = new Set<string>();
  for (const m of tx.movements) {
    const to = m.to?.value;
    const from = m.from?.value;
    if (own(to) && !own(from)) {
      credit += atomic(m.amount, decimals);
      if (from) others.add(from);
    } else if (own(from) && !own(to)) {
      debit += atomic(m.amount, decimals);
      if (to) others.add(to);
    }
  }
  const fee = tx.fee && own(tx.fee.payer?.value) ? tx.fee : null;
  return { credit, debit, net: credit - debit, counterparties: [...others], fee };
}

export function shorten(text: string): string {
  return text.length > 20 ? `${text.slice(0, 9)}…${text.slice(-6)}` : text;
}
