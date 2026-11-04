import Box from '@mui/material/Box';
import MenuItem from '@mui/material/MenuItem';
import TextField from '@mui/material/TextField';
import type { SxProps, Theme } from '@mui/material/styles';
import { Currency } from './currency';

/** One network to pick: `chain/network`, and its native currency's ticker for the icon. */
export interface NetworkChoice {
  value: string;
  ticker: string | null;
}

/** `chain/network` as an icon, the chain, then the network in muted text. */
function Name({ choice }: { choice: NetworkChoice }) {
  const [chain = '', network = ''] = choice.value.split('/');
  return (
    <Box
      component="span"
      sx={{ display: 'inline-flex', alignItems: 'center', gap: 1, lineHeight: 'inherit' }}
    >
      <Box component="span" sx={{ display: 'flex', fontSize: 15 }}>
        {choice.ticker ? <Currency chain={chain} ticker={choice.ticker} label={false} /> : null}
      </Box>
      <span>{chain}</span>
      <Box component="span" sx={{ color: 'text.secondary' }}>
        {network}
      </Box>
    </Box>
  );
}

/** A network picker: each network with its currency's symbol. */
export function NetworkSelect({
  value,
  networks,
  onChange,
  label = 'Network',
  required = false,
  sx,
}: {
  value: string;
  networks: NetworkChoice[];
  onChange: (value: string) => void;
  label?: string;
  required?: boolean;
  sx?: SxProps<Theme>;
}) {
  return (
    <TextField
      select
      label={label}
      value={value}
      required={required}
      onChange={(e) => onChange(e.target.value)}
      slotProps={{
        select: {
          // The icon must not push the value off the field's centre line.
          SelectDisplayProps: { style: { display: 'flex', alignItems: 'center' } },
          renderValue: (selected) => {
            const choice = networks.find((n) => n.value === selected);
            return choice ? <Name choice={choice} /> : String(selected);
          },
        },
      }}
      {...(sx ? { sx } : {})}
    >
      {networks.map((n) => (
        <MenuItem key={n.value} value={n.value}>
          <Name choice={n} />
        </MenuItem>
      ))}
    </TextField>
  );
}
