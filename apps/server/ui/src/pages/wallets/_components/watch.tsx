import Event from '@mui/icons-material/Event';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Dialog from '@mui/material/Dialog';
import DialogActions from '@mui/material/DialogActions';
import DialogContent from '@mui/material/DialogContent';
import DialogContentText from '@mui/material/DialogContentText';
import DialogTitle from '@mui/material/DialogTitle';
import IconButton from '@mui/material/IconButton';
import MenuItem from '@mui/material/MenuItem';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import ToggleButton from '@mui/material/ToggleButton';
import ToggleButtonGroup from '@mui/material/ToggleButtonGroup';
import Tooltip from '@mui/material/Tooltip';
import Typography from '@mui/material/Typography';
import { useRef, useState, type SubmitEvent } from 'react';
import { ApiError, call, client, type Listing, type Wallet } from '@/api/client';
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
  const network = offered[0]?.network;
  const key = network ? `${network.chain}/${network.name}` : '';
  const [picking, setPicking] = useState(false);
  const [note, setNote] = useState<{ text: string; error: boolean } | null>(null);
  // The network whose tip endpoint answered that it cannot find blocks by time.
  const [undated, setUndated] = useState<string | null>(null);
  const byTime = network !== undefined && undated !== key;
  // Each pick's number: only the latest pick's answer fills the field.
  const picks = useRef(0);
  // How old, in days, a start may be on the network `key`, and the earliest
  // time that allowed when it was learned (null: no limit).
  const [limit, setLimit] = useState<{
    key: string;
    days: number | null;
    earliest: number | null;
  } | null>(null);
  const days = limit?.key === key ? limit.days : null;
  const earliest = limit?.key === key ? limit.earliest : null;

  /** Opens or closes the time picker; learns the network's history limit once. */
  const pickByTime = () => {
    setPicking(!picking);
    if (!network || limit?.key === key) return;
    call(client.GET('/v1/networks'))
      .then((list) => {
        const found = list.networks.find(
          (n) => n.network.chain === network.chain && n.network.name === network.name,
        );
        const days = found?.max_history_days ?? null;
        const earliest = days === null ? null : Date.now() - days * 86_400_000;
        setLimit({ key, days, earliest });
      })
      .catch(() => setLimit({ key, days: null, earliest: null }));
  };

  /** Fills the start block with the last block at or before `local` (a datetime-local value). */
  const fill = (local: string) => {
    const at = Math.floor(new Date(local).getTime() / 1000);
    if (!network || !Number.isFinite(at)) return;
    if (earliest !== null && at * 1000 < earliest) {
      setNote({ text: `older than this network's ${days}-day history limit`, error: true });
      return;
    }
    const path = { chain: network.chain, network: network.name };
    const pick = ++picks.current;
    call(
      client.GET('/v1/networks/{chain}/{network}/tip', {
        params: { path, query: { at: `${at}` } },
      }),
    )
      .then((tip) => {
        if (pick !== picks.current) return;
        onChange({ ...value, from: `${tip.position}` });
        setNote({
          text:
            tip.timestamp === null
              ? `block ${tip.position}`
              : `block from ${new Date(tip.timestamp * 1000).toLocaleString(undefined, {
                  dateStyle: 'medium',
                  timeStyle: 'short',
                })}`,
          error: false,
        });
      })
      .catch((error: unknown) => {
        if (pick !== picks.current) return;
        if (error instanceof ApiError && error.status === 422) {
          setUndated(key);
          setPicking(false);
        }
        setNote({ text: error instanceof Error ? error.message : String(error), error: true });
      });
  };

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
          sx={{ flex: 'none', '& .MuiToggleButton-root': { whiteSpace: 'nowrap' } }}
          value={value.start}
          onChange={(_, start: Watching['start'] | null) => start && onChange({ ...value, start })}
        >
          <ToggleButton value="fresh">From now</ToggleButton>
          <ToggleButton value="at">From block</ToggleButton>
        </ToggleButtonGroup>
        {value.start === 'at' && (
          <TextField
            size="small"
            fullWidth={false}
            label="Start block"
            value={value.from}
            onChange={(e) => {
              setNote(null);
              onChange({ ...value, from: e.target.value.replace(/\D/g, '') });
            }}
            slotProps={{ htmlInput: { inputMode: 'numeric' } }}
            required
            sx={{ width: 150 }}
          />
        )}
        {value.start === 'at' && byTime && (
          <Tooltip title="Fill by time">
            <IconButton size="small" onClick={pickByTime}>
              <Event fontSize="small" />
            </IconButton>
          </Tooltip>
        )}
      </Stack>
      {value.start === 'at' && byTime && picking && (
        <TextField
          size="small"
          type="datetime-local"
          label="Last block at or before"
          onChange={(e) => fill(e.target.value)}
          slotProps={{
            inputLabel: { shrink: true },
            ...(earliest !== null && { htmlInput: { min: local(earliest) } }),
          }}
        />
      )}
      {value.start === 'at' && note && (
        <Typography variant="caption" color={note.error ? 'error' : 'text.secondary'}>
          {note.text}
        </Typography>
      )}
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

/** `millis` as a `datetime-local` value, in local time. */
function local(millis: number): string {
  const offset = new Date(millis).getTimezoneOffset() * 60_000;
  return new Date(millis - offset).toISOString().slice(0, 16);
}
