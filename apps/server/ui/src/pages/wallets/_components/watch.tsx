import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Dialog from '@mui/material/Dialog';
import DialogActions from '@mui/material/DialogActions';
import DialogContent from '@mui/material/DialogContent';
import DialogContentText from '@mui/material/DialogContentText';
import DialogTitle from '@mui/material/DialogTitle';
import MenuItem from '@mui/material/MenuItem';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import ToggleButton from '@mui/material/ToggleButton';
import ToggleButtonGroup from '@mui/material/ToggleButtonGroup';
import { useState, type SubmitEvent } from 'react';
import { call, client, type Listing, type Wallet } from '@/api/client';
import type { Shell } from '@/layout';
import { WATCH_NOTHING, watchBody, type Watching } from './watching';

/** The assets to index in, and the birthday to index from. */
export function WatchFields({
  offered,
  value,
  onChange,
}: {
  /** The network's configured assets. */
  offered: Listing[];
  value: Watching;
  onChange: (value: Watching) => void;
}) {
  return (
    <Stack spacing={2}>
      <TextField
        select
        label="Assets to watch"
        value={value.assets}
        onChange={(e) => {
          const picked = e.target.value as unknown as string[] | string;
          onChange({ ...value, assets: typeof picked === 'string' ? picked.split(',') : picked });
        }}
        slotProps={{ select: { multiple: true } }}
        required
      >
        {offered.map((l) => (
          <MenuItem key={l.asset} value={l.asset}>
            {l.metadata.ticker ?? l.asset}
          </MenuItem>
        ))}
      </TextField>
      <Stack direction="row" spacing={1.5} sx={{ alignItems: 'center' }}>
        <ToggleButtonGroup
          size="small"
          exclusive
          value={value.start}
          onChange={(_, start: Watching['start'] | null) => start && onChange({ ...value, start })}
        >
          <ToggleButton value="fresh">From now</ToggleButton>
          <ToggleButton value="at">From block</ToggleButton>
        </ToggleButtonGroup>
        {value.start === 'at' && (
          <TextField
            size="small"
            label="Birthday: block position"
            value={value.from}
            onChange={(e) => onChange({ ...value, from: e.target.value.replace(/\D/g, '') })}
            slotProps={{ htmlInput: { inputMode: 'numeric' } }}
            required
          />
        )}
      </Stack>
    </Stack>
  );
}

/** Starts watching `wallet` in more assets. */
export function WatchDialog({
  shell,
  wallet,
  open,
  onClose,
  onDone,
}: {
  shell: Shell;
  wallet: Wallet;
  open: boolean;
  onClose: () => void;
  onDone: (wallet: Wallet) => void;
}) {
  const [watching, setWatching] = useState<Watching>(WATCH_NOTHING);
  const [busy, setBusy] = useState(false);
  const offered = shell.listings.filter(
    (l) =>
      l.network.chain === wallet.network.chain &&
      l.network.name === wallet.network.name &&
      !wallet.assets.includes(l.asset),
  );
  return (
    <Dialog open={open} onClose={onClose} fullWidth maxWidth="xs">
      <Box
        component="form"
        onSubmit={(event: SubmitEvent<HTMLFormElement>) => {
          event.preventDefault();
          setBusy(true);
          const body = {
            chain: wallet.network.chain,
            network: wallet.network.name,
            address: wallet.address,
            ...watchBody(watching),
          };
          call(client.POST('/v1/wallets', { body }))
            .then((watched) => {
              setWatching(WATCH_NOTHING);
              onDone(watched);
            })
            .catch(shell.fail)
            .finally(() => setBusy(false));
        }}
      >
        <DialogTitle variant="h3">Watch</DialogTitle>
        <DialogContent>
          <DialogContentText sx={{ fontSize: 13, mb: 2 }}>
            Indexes this address in the chosen assets from its birthday. Its key, if kept here,
            stays.
          </DialogContentText>
          <WatchFields offered={offered} value={watching} onChange={setWatching} />
        </DialogContent>
        <DialogActions>
          <Button color="inherit" onClick={onClose}>
            Cancel
          </Button>
          <Button
            type="submit"
            variant="contained"
            loading={busy}
            disabled={watching.assets.length === 0}
          >
            Watch
          </Button>
        </DialogActions>
      </Box>
    </Dialog>
  );
}
