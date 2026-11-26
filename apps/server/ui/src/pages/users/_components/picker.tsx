import Box from '@mui/material/Box';
import Checkbox from '@mui/material/Checkbox';
import FormControlLabel from '@mui/material/FormControlLabel';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useState } from 'react';
import { mono } from '@/theme';
import { ROOT, allows, full, pack, readable, short, type Resources } from './scopes';

/**
 * Scopes as checkboxes by resource, only `held` ones enabled, plus scopes by
 * id as text; starts at `initial` and reports every change to `onChange`.
 */
export function Picker({
  resources,
  held,
  initial,
  onChange,
}: {
  resources: Resources;
  held: string[];
  initial: string[];
  onChange: (scopes: string[]) => void;
}) {
  const all = resources.flatMap(([resource, actions]) =>
    actions.map((action) => `${ROOT}.${resource}.$${action}`),
  );
  const [checked, setChecked] = useState(() => all.filter((scope) => allows(initial, scope)));
  const [extra, setExtra] = useState(() =>
    initial
      .filter((scope) => scope.includes('['))
      .map(short)
      .join(' '),
  );
  const scopes = [...checked, ...extra.split(/\s+/).filter(Boolean).map(full)];

  const change = (nextChecked: string[], nextExtra: string) => {
    setChecked(nextChecked);
    setExtra(nextExtra);
    onChange([...nextChecked, ...nextExtra.split(/\s+/).filter(Boolean).map(full)]);
  };

  return (
    <Box>
      <Box sx={{ border: 1, borderColor: 'divider', borderRadius: 1, px: 1.5, py: 0.5 }}>
        {resources.map(([resource, actions]) => (
          <Stack key={resource} direction="row" sx={{ alignItems: 'center' }}>
            <Typography sx={{ fontFamily: mono, fontSize: 12.5, width: 120, flex: 'none' }}>
              {resource}
            </Typography>
            {actions.map((action) => {
              const scope = `${ROOT}.${resource}.$${action}`;
              return (
                <FormControlLabel
                  key={action}
                  label={action}
                  disabled={!allows(held, scope)}
                  slotProps={{ typography: { sx: { fontSize: 13 } } }}
                  control={
                    <Checkbox
                      size="small"
                      checked={checked.includes(scope)}
                      onChange={(e) =>
                        change(
                          e.target.checked
                            ? [...checked, scope]
                            : checked.filter((s) => s !== scope),
                          extra,
                        )
                      }
                    />
                  }
                />
              );
            })}
          </Stack>
        ))}
      </Box>
      <TextField
        label="By id"
        value={extra}
        onChange={(e) => change(checked, e.target.value)}
        helperText="Space-separated, e.g. wallets[ethereum.mainnet.0xabc].$read"
        slotProps={{ htmlInput: { spellCheck: false, style: { fontFamily: mono } } }}
        sx={{ mt: 2 }}
      />
      <Typography
        sx={{ mt: 1.5, fontFamily: mono, fontSize: 11.5, color: 'text.secondary' }}
        aria-label="Packed scopes"
      >
        {JSON.stringify(pack(scopes))}
      </Typography>
    </Box>
  );
}

/** Scopes one per line: as written, and what each allows. */
export function ScopeList({ scopes }: { scopes: string[] }) {
  if (scopes.length === 0)
    return (
      <Typography color="text.secondary" sx={{ fontSize: 13 }}>
        None.
      </Typography>
    );
  return (
    <Box component="ul" sx={{ m: 0, p: 0, listStyle: 'none' }}>
      {scopes.map((scope) => (
        <Stack component="li" key={scope} direction="row" spacing={1.5} sx={{ py: 0.25 }}>
          <Typography sx={{ fontFamily: mono, fontSize: 12.5, wordBreak: 'break-all' }}>
            {short(scope)}
          </Typography>
          <Typography color="text.secondary" sx={{ fontSize: 12.5 }}>
            {readable(scope)}
          </Typography>
        </Stack>
      ))}
    </Box>
  );
}
