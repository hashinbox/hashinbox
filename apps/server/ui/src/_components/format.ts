// The one way an amount is written for people: every view uses `format`, as
// the server's `AssetInfo::format` does for logs. Exact: never a float.
import Decimal from 'decimal.js';

/** Decimal arithmetic wide enough that no amount is ever rounded by it. */
const Exact = Decimal.clone({ precision: 1000, toExpNeg: -9e15, toExpPos: 9e15 });

const THIN = ' ';

/**
 * `amount` as a view shows it: cut toward zero to exactly `display`
 * fractional digits (all it has when absent), thousands grouped by thin
 * spaces, a true minus.
 * With `decimals`, `amount` is atomic units of an asset with that many.
 */
export function format(
  amount: Decimal.Value,
  {
    decimals,
    display,
  }: { decimals?: number | undefined; display?: number | null | undefined } = {},
): string {
  let value = new Exact(amount);
  if (decimals !== undefined) value = value.div(new Exact(10).pow(decimals));
  if (display != null) value = value.toDecimalPlaces(display, Exact.ROUND_DOWN);
  // With `display`, always that many fractional digits: amounts of one asset line up.
  const fixed = display != null ? value.abs().toFixed(display) : value.abs().toFixed();
  const [whole = '0', fraction] = fixed.split('.');
  const grouped = whole.replace(/\B(?=(\d{3})+(?!\d))/g, THIN);
  return `${value.isNegative() && !value.isZero() ? '−' : ''}${grouped}${fraction ? `.${fraction}` : ''}`;
}
