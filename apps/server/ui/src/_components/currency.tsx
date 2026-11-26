import Box from '@mui/material/Box';
import { useState } from 'react';

/** Each chain's short name in icon file names. */
const SHORT: Record<string, string> = {
  bitcoin: 'btc',
  bnb: 'bnb',
  ethereum: 'eth',
  solana: 'sol',
  tron: 'trx',
};

/**
 * A network asset by its chain and ticker: its icon, served at
 * `/assets/crypto/<chain>.svg` for the native coin (its ticker is the chain's
 * short name, e.g. `eth`) and `/assets/crypto/<chain>_<ticker>.svg` for a token
 * (e.g. `eth_usdc`), then the ticker unless `label` is false. Without an icon
 * the ticker always shows.
 */
export function Currency({
  chain,
  ticker,
  label = true,
}: {
  chain: string;
  ticker: string;
  label?: boolean;
}) {
  const short = SHORT[chain];
  const [failed, setFailed] = useState(false);
  const missing = failed || !short;
  const lower = ticker.toLowerCase();
  const name = lower === short ? short : `${short}_${lower}`;
  return (
    <Box
      component="span"
      title={ticker}
      sx={{ display: 'inline-flex', alignItems: 'center', gap: 0.5, verticalAlign: 'middle' }}
    >
      {!missing && (
        <Box
          component="img"
          src={`/assets/crypto/${name}.svg`}
          alt={label ? '' : ticker}
          onError={() => setFailed(true)}
          sx={{ width: '1.15em', height: '1.15em', flex: 'none' }}
        />
      )}
      {(label || missing) && <span>{ticker}</span>}
    </Box>
  );
}

/** Several assets of `chain` as one mark: the first in front, the rest tucked behind, each ringed. */
export function Currencies({ chain, tickers }: { chain: string; tickers: string[] }) {
  return (
    <Box sx={{ display: 'flex', flex: 'none', width: '1.6em', isolation: 'isolate' }}>
      {tickers.map((ticker, i) => (
        <Box
          key={ticker}
          sx={(t) => ({
            display: 'flex',
            ml: i === 0 ? 0 : '-0.75em',
            zIndex: tickers.length - i,
            borderRadius: '50%',
            boxShadow: `0 0 0 1.5px ${t.palette.background.paper}`,
          })}
        >
          <Currency chain={chain} ticker={ticker} label={false} />
        </Box>
      ))}
    </Box>
  );
}
