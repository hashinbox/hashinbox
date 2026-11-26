import Search from '@mui/icons-material/Search';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Dialog from '@mui/material/Dialog';
import DialogActions from '@mui/material/DialogActions';
import DialogContent from '@mui/material/DialogContent';
import DialogContentText from '@mui/material/DialogContentText';
import DialogTitle from '@mui/material/DialogTitle';
import IconButton from '@mui/material/IconButton';
import InputAdornment from '@mui/material/InputAdornment';
import MenuItem from '@mui/material/MenuItem';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import ToggleButton from '@mui/material/ToggleButton';
import ToggleButtonGroup from '@mui/material/ToggleButtonGroup';
import Tooltip from '@mui/material/Tooltip';
import Typography from '@mui/material/Typography';
import { useState, type SubmitEvent } from 'react';
import type { Listing, Wallet } from '@hashinbox/client';
import type { Shell } from '@/layout';
import { SearchDialog } from './search';
import { WATCH_NOTHING, watch, type Watching } from './watching';

/** The assets to index in, and the block to index from. */
export function WatchFields({
  offered,
  value,
  onChange,
  freshOnly = false,
}: {
  /** The network's configured assets. */
  offered: Listing[];
  value: Watching;
  onChange: (value: Watching) => void;
  /** A key generated here has no past: it is indexed from now only. */
  freshOnly?: boolean;
}) {
  const network = offered[0]?.network;
  const [searching, setSearching] = useState(false);

  return (
    <Stack spacing={2}>
      <TextField
        select
        label="Assets to index"
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
      {freshOnly ? (
        <Typography variant="caption" color="text.secondary">
          A new key has no past: it is indexed from now.
        </Typography>
      ) : (
        <Stack direction="row" spacing={1.5} sx={{ alignItems: 'center' }}>
          <ToggleButtonGroup
            size="small"
            exclusive
            sx={{ flex: 'none', '& .MuiToggleButton-root': { whiteSpace: 'nowrap' } }}
            value={value.start}
            onChange={(_, start: Watching['start'] | null) =>
              start && onChange({ ...value, start })
            }
          >
            <ToggleButton value="fresh">From now</ToggleButton>
            <ToggleButton value="at">From block</ToggleButton>
          </ToggleButtonGroup>
          {value.start === 'at' && (
            <TextField
              size="small"
              label="Start block"
              value={value.from}
              onChange={(e) => onChange({ ...value, from: e.target.value.replace(/\D/g, '') })}
              slotProps={{
                htmlInput: { inputMode: 'numeric' },
                input: {
                  endAdornment: network && (
                    <InputAdornment position="end">
                      <Tooltip title="Find by time">
                        <IconButton
                          size="small"
                          edge="end"
                          aria-label="Find the block by time"
                          onClick={() => setSearching(true)}
                        >
                          <Search fontSize="small" />
                        </IconButton>
                      </Tooltip>
                    </InputAdornment>
                  ),
                },
              }}
              required
              sx={{ width: 180 }}
            />
          )}
        </Stack>
      )}
      {network && (
        <SearchDialog
          network={network}
          open={searching}
          onClose={() => setSearching(false)}
          onUse={(position) => {
            onChange({ ...value, from: `${position}` });
            setSearching(false);
          }}
        />
      )}
    </Stack>
  );
}

/** Indexes `wallet` in more assets: step 2 of adding a wallet, alone. */
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
      l.network.chain === wallet.chain &&
      l.network.name === wallet.network &&
      !(l.asset in wallet.indexes),
  );
  return (
    <Dialog open={open} onClose={onClose} fullWidth maxWidth="xs">
      <Box
        component="form"
        onSubmit={(event: SubmitEvent<HTMLFormElement>) => {
          event.preventDefault();
          setBusy(true);
          watch(wallet.id, watching)
            .then((watched) => {
              setWatching(WATCH_NOTHING);
              onDone(watched);
            })
            .catch(shell.fail)
            .finally(() => setBusy(false));
        }}
      >
        <DialogTitle variant="h3">Watch more assets</DialogTitle>
        <DialogContent>
          <DialogContentText sx={{ fontSize: 13, mb: 2 }}>
            Indexes this wallet in the chosen assets from the start block.
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
