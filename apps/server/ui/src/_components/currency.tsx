import Box from '@mui/material/Box';
import { useTheme } from '@mui/material/styles';
import { useState } from 'react';

/**
 * A currency by its ticker: its symbol, served at
 * `/assets/currencies/<ticker>.svg`, then the ticker unless `label` is false.
 * Without a symbol the ticker always shows.
 */
export function Currency({ ticker, label = true }: { ticker: string; label?: boolean }) {
  const theme = useTheme();
  const [missing, setMissing] = useState(false);
  return (
    <Box
      component="span"
      title={ticker}
      sx={{ display: 'inline-flex', alignItems: 'center', gap: 0.5, verticalAlign: 'middle' }}
    >
      {!missing && (
        <Box
          component="img"
          src={`/assets/currencies/${ticker.toLowerCase()}.svg`}
          alt={label ? '' : ticker}
          onError={() => setMissing(true)}
          sx={{
            width: '1.15em',
            height: '1.15em',
            flex: 'none',
            // The black symbol, faded to light gray (inverted on a dark page).
            opacity: 0.35,
            filter: theme.palette.mode === 'dark' ? 'invert(1)' : undefined,
          }}
        />
      )}
      {(label || missing) && <span>{ticker}</span>}
    </Box>
  );
}
