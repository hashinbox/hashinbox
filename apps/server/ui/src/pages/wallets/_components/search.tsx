// Finds the block a network had at a time, showing the search narrow as the
// server streams it (`GET /v1/networks/{chain}/{network}/tip/search`).
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Dialog from '@mui/material/Dialog';
import DialogActions from '@mui/material/DialogActions';
import DialogContent from '@mui/material/DialogContent';
import DialogContentText from '@mui/material/DialogContentText';
import DialogTitle from '@mui/material/DialogTitle';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useEffect, useRef, useState, type SubmitEvent } from 'react';
import { call, client, events } from '@/api/client';
import { mono } from '@/theme';

/** A produced block: its native position and Unix seconds. */
interface Stamp {
  position: number;
  timestamp: number;
}

/** One step of the search: the answer is at or after `low`, before `high`. */
interface Probe {
  low: Stamp;
  high: Stamp;
}

/** The network searched. */
interface Network {
  chain: string;
  name: string;
}

/** Searches `network` by time; `onUse` takes the block found. */
export function SearchDialog({
  network,
  open,
  onClose,
  onUse,
}: {
  network: Network;
  open: boolean;
  onClose: () => void;
  onUse: (position: number) => void;
}) {
  const [at, setAt] = useState('');
  const [probes, setProbes] = useState<Probe[]>([]);
  const [found, setFound] = useState<Stamp | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // The earliest time the network's history limit allows (null: no limit).
  const [earliest, setEarliest] = useState<number | null>(null);
  const searching = useRef<AbortController | null>(null);
  const target = at === '' ? NaN : Math.floor(new Date(at).getTime() / 1000);

  useEffect(() => {
    if (!open) return;
    let live = true;
    call(client.GET('/v1/networks'))
      .then((list) => {
        const days = list.networks.find(
          (n) => n.chain === network.chain && n.network === network.name,
        )?.max_history_days;
        if (live)
          setEarliest(days === undefined || days === null ? null : Date.now() - days * 86_400_000);
      })
      .catch(() => live && setEarliest(null));
    return () => {
      live = false;
      searching.current?.abort();
    };
  }, [open, network.chain, network.name]);

  const search = () => {
    searching.current?.abort();
    const controller = new AbortController();
    searching.current = controller;
    setProbes([]);
    setFound(null);
    setError(null);
    setBusy(true);
    const path =
      `/v1/networks/${encodeURIComponent(network.chain)}/${encodeURIComponent(network.name)}` +
      `/tip/search?at=${target}`;
    events(
      path,
      ({ event, data }) => {
        if (event === 'probe') setProbes((all) => [...all, data as Probe]);
        else if (event === 'found') setFound(data as Stamp);
        else if (event === 'error') setError((data as { message: string }).message);
      },
      controller.signal,
    )
      .catch((failure: unknown) => {
        if (!controller.signal.aborted)
          setError(failure instanceof Error ? failure.message : String(failure));
      })
      .finally(() => {
        if (searching.current === controller) setBusy(false);
      });
  };

  const tooOld = earliest !== null && target * 1000 < earliest;
  return (
    <Dialog open={open} onClose={onClose} fullWidth maxWidth="xs">
      <Box
        component="form"
        onSubmit={(event: SubmitEvent<HTMLFormElement>) => {
          event.preventDefault();
          search();
        }}
      >
        <DialogTitle variant="h3">Find a block by time</DialogTitle>
        <DialogContent>
          <DialogContentText sx={{ fontSize: 13, mb: 2 }}>
            The last block at or before the time, found by a binary search over the node&apos;s
            blocks.
          </DialogContentText>
          <Stack direction="row" spacing={1.5} sx={{ alignItems: 'center' }}>
            <TextField
              size="small"
              type="datetime-local"
              label="At or before"
              value={at}
              onChange={(e) => setAt(e.target.value)}
              error={tooOld}
              slotProps={{
                inputLabel: { shrink: true },
                ...(earliest !== null && { htmlInput: { min: local(earliest) } }),
              }}
              sx={{ flexGrow: 1 }}
            />
            <Button
              type="submit"
              variant="outlined"
              loading={busy}
              disabled={!Number.isFinite(target) || tooOld}
            >
              Search
            </Button>
          </Stack>
          {tooOld && (
            <Typography variant="caption" color="error">
              Older than this network&apos;s history limit.
            </Typography>
          )}
          {probes.length > 0 && (
            <Box
              sx={{
                mt: 2,
                maxHeight: 220,
                overflowY: 'auto',
                fontFamily: mono,
                fontSize: 12,
                color: 'text.secondary',
              }}
            >
              {probes.map((p, i) => (
                <div key={i}>
                  #{p.low.position} … #{p.high.position} · {p.high.position - p.low.position} blocks
                  · {off(p.low.timestamp - target)} … {off(p.high.timestamp - target)}
                </div>
              ))}
            </Box>
          )}
          {found && (
            <Typography sx={{ mt: 2, fontSize: 13 }}>
              Block{' '}
              <Box component="span" sx={{ fontFamily: mono }}>
                #{found.position}
              </Box>{' '}
              from{' '}
              {new Date(found.timestamp * 1000).toLocaleString(undefined, {
                dateStyle: 'medium',
                timeStyle: 'short',
              })}{' '}
              ({off(found.timestamp - target)})
            </Typography>
          )}
          {error && (
            <Typography sx={{ mt: 2, fontSize: 13 }} color="error">
              {error}
            </Typography>
          )}
        </DialogContent>
        <DialogActions>
          <Button color="inherit" onClick={onClose}>
            Cancel
          </Button>
          <Button
            variant="contained"
            disabled={!found}
            onClick={() => found && onUse(found.position)}
          >
            Use
          </Button>
        </DialogActions>
      </Box>
    </Dialog>
  );
}

/** How far a block's time is from the time asked for: `3d before`, `12m after`, `on time`. */
function off(seconds: number): string {
  if (seconds === 0) return 'on time';
  const s = Math.abs(seconds);
  const [size, unit] =
    (
      [
        [86_400, 'd'],
        [3_600, 'h'],
        [60, 'm'],
      ] as const
    ).find(([n]) => s >= n) ?? ([1, 's'] as const);
  return `${Math.floor(s / size)}${unit} ${seconds < 0 ? 'before' : 'after'}`;
}

/** `millis` as a `datetime-local` value, in local time. */
function local(millis: number): string {
  const offset = new Date(millis).getTimezoneOffset() * 60_000;
  return new Date(millis - offset).toISOString().slice(0, 16);
}
