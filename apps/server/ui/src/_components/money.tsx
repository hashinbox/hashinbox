import Box from '@mui/material/Box';
import type Decimal from 'decimal.js';
import { resolve, type Action, type Actions } from './actions';
import { Currency } from './currency';
import { format } from './format';
import { Hover } from './hover';

/**
 * An amount with its asset's icon on `chain` and ticker, cut to `display`
 * fractional digits. With `decimals`, `amount` is atomic units. Hovered, it
 * offers Copy of the exact amount; `actions` replaces or extends that.
 */
export function Money({
  amount,
  chain,
  ticker,
  decimals,
  display,
  actions,
}: {
  amount: Decimal.Value;
  chain: string;
  ticker?: string | null;
  decimals?: number;
  display?: number | null;
  actions?: Actions;
}) {
  const exact = format(amount, { decimals });
  const defaults: Action[] = [
    { label: 'Copy', copy: exact.replace(/\u2009/g, '').replace('−', '-') },
  ];
  return (
    <Hover actions={resolve(defaults, actions)}>
      <Box
        component="span"
        sx={{
          display: 'inline-flex',
          alignItems: 'center',
          gap: 0.5,
          whiteSpace: 'nowrap',
          fontVariantNumeric: 'tabular-nums',
          verticalAlign: 'middle',
        }}
      >
        {ticker && <Currency chain={chain} ticker={ticker} label={false} />}
        <span>{format(amount, { decimals, display })}</span>
        {ticker && (
          <Box component="span" sx={{ color: 'text.secondary' }}>
            {ticker}
          </Box>
        )}
      </Box>
    </Hover>
  );
}
