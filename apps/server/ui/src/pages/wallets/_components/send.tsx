import CheckCircle from '@mui/icons-material/CheckCircle';
import Cancel from '@mui/icons-material/Cancel';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import CircularProgress from '@mui/material/CircularProgress';
import Dialog from '@mui/material/Dialog';
import DialogActions from '@mui/material/DialogActions';
import DialogContent from '@mui/material/DialogContent';
import DialogTitle from '@mui/material/DialogTitle';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useEffect, useState, type ReactNode, type SubmitEvent } from 'react';
import { call, client, type Wallet } from '@/api/client';
import { Transaction } from '@/_components/transaction';
import type { Shell } from '@/layout';
import { assetName, networkOf } from '@/money';

/** How often a sent transaction is looked up again. */
const POLL_MS = 3000;

/**
 * Where a send is: being filled in; queued, until the nodes accept it;
 * accepted, until the wallet's index sees it mined; mined at `block`, until
 * the index calls that block final; final; or failed with why.
 */
type Stage =
  | { at: 'form' }
  | { at: 'queued'; job: string }
  | { at: 'accepted'; job: string; txids: string[] }
  | { at: 'mined'; txid: string; block: number }
  | { at: 'final'; txid: string; block: number }
  | { at: 'failed'; reason: string };

/** Sends `asset` of `wallet`, then follows it until its block is final. */
export function Send({
  shell,
  wallet,
  asset,
  open,
  onClose,
  onSent,
}: {
  shell: Shell;
  wallet: Wallet;
  asset: string;
  open: boolean;
  onClose: () => void;
  /** Called once the send is mined, and again once it is final. */
  onSent: () => void;
}) {
  const ticker = assetName(shell.listings, wallet.chain, wallet.network, asset);
  const [destination, setDestination] = useState('');
  const [amount, setAmount] = useState('');
  const [busy, setBusy] = useState(false);
  const [stage, setStage] = useState<Stage>({ at: 'form' });

  const submit = (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    setBusy(true);
    const params = { path: { id: wallet.id } };
    const body = { asset, transfers: [{ destination: destination.trim(), amount: amount.trim() }] };
    call(client.POST('/v1/wallets/{id}/transactions', { params, body }))
      .then((queued) => setStage({ at: 'queued', job: queued.job }))
      .catch(shell.fail)
      .finally(() => setBusy(false));
  };

  // Each stage looks up what ends it, until the send is final or failed.
  useEffect(() => {
    if (stage.at === 'form' || stage.at === 'final' || stage.at === 'failed') return;
    const look = async (): Promise<Stage | null> => {
      if (stage.at === 'queued') {
        const job = await call(
          client.GET('/v1/jobs/{id}', { params: { path: { id: stage.job } } }),
        );
        if (job.state === 'failed') return { at: 'failed', reason: job.reason ?? 'failed' };
        if (job.state !== 'succeeded') return null;
        // A broadcast's progress names its transactions in `ids`.
        const ids: unknown = job.progress && 'ids' in job.progress ? job.progress.ids : [];
        return { at: 'accepted', job: stage.job, txids: Array.isArray(ids) ? ids.map(String) : [] };
      }
      if (stage.at === 'accepted') {
        const params = { path: { id: wallet.id, asset } };
        const page = await call(client.GET('/v1/wallets/{id}/indexes/{asset}/legs', { params }));
        const leg = page.legs.find((l) => l.txid != null && stage.txids.includes(l.txid));
        if (!leg?.txid || leg.position == null) return null;
        onSent();
        return { at: 'mined', txid: leg.txid, block: leg.position };
      }
      const fresh = await call(
        client.GET('/v1/wallets/{id}', { params: { path: { id: wallet.id } } }),
      );
      const final = fresh.indexes[asset]?.finality_at;
      if (final == null || final < stage.block) return null;
      onSent();
      return { at: 'final', txid: stage.txid, block: stage.block };
    };
    let stopped = false;
    const timer = window.setInterval(() => {
      look()
        .then((next) => {
          if (!stopped && next) setStage(next);
        })
        .catch(shell.fail);
    }, POLL_MS);
    return () => {
      stopped = true;
      window.clearInterval(timer);
    };
  }, [stage, wallet.id, asset, onSent, shell.fail]);

  const close = () => {
    onClose();
    // A finished send starts over; one in flight keeps being followed when reopened.
    if (stage.at === 'final' || stage.at === 'failed') {
      setStage({ at: 'form' });
      setDestination('');
      setAmount('');
    }
  };

  const txid = stage.at === 'mined' || stage.at === 'final' ? stage.txid : null;
  const order = ['queued', 'accepted', 'mined', 'final'];
  const reached = order.indexOf(stage.at);

  return (
    <Dialog open={open} onClose={close} maxWidth="sm" fullWidth>
      <DialogTitle>Send {ticker}</DialogTitle>
      {stage.at === 'form' ? (
        <Box component="form" autoComplete="off" onSubmit={submit}>
          <DialogContent>
            <Stack spacing={2.5} sx={{ pt: 1 }}>
              <TextField
                autoFocus
                label="To"
                value={destination}
                onChange={(e) => setDestination(e.target.value)}
                slotProps={{ htmlInput: { spellCheck: false } }}
                required
              />
              <TextField
                label={`Amount (${ticker})`}
                placeholder="0.00"
                value={amount}
                onChange={(e) => setAmount(e.target.value)}
                required
                slotProps={{ htmlInput: { inputMode: 'decimal' } }}
              />
            </Stack>
          </DialogContent>
          <DialogActions>
            <Button variant="text" onClick={close}>
              Cancel
            </Button>
            <Button type="submit" variant="contained" loading={busy}>
              Send
            </Button>
          </DialogActions>
        </Box>
      ) : (
        <>
          <DialogContent>
            <Typography sx={{ mb: 2 }}>
              {amount} {ticker} to {destination}
            </Typography>
            {stage.at === 'failed' ? (
              <Step state="failed" label={`Failed: ${stage.reason}`} />
            ) : (
              <Stack spacing={1.25}>
                <Step
                  state={reached > 0 ? 'done' : 'busy'}
                  label={reached > 0 ? 'Accepted by the network' : 'Broadcasting'}
                />
                <Step
                  state={reached > 1 ? 'done' : reached === 1 ? 'busy' : 'todo'}
                  label={
                    reached > 1 && 'block' in stage
                      ? `Mined in block #${stage.block}`
                      : 'Waiting to be mined'
                  }
                />
                <Step
                  state={reached > 2 ? 'done' : reached === 2 ? 'busy' : 'todo'}
                  label={reached > 2 ? 'Final' : 'Waiting until final'}
                />
              </Stack>
            )}
            {txid && (
              <Box sx={{ mt: 2, fontSize: 13 }}>
                <Transaction value={txid} network={networkOf(wallet)} trim={false} />
              </Box>
            )}
          </DialogContent>
          <DialogActions>
            <Button variant={stage.at === 'final' ? 'contained' : 'text'} onClick={close}>
              {stage.at === 'final' || stage.at === 'failed' ? 'Done' : 'Hide'}
            </Button>
          </DialogActions>
        </>
      )}
    </Dialog>
  );
}

/** One step of a send: done, under way, not yet, or failed. */
function Step({ state, label }: { state: 'done' | 'busy' | 'todo' | 'failed'; label: string }) {
  const icon: Record<typeof state, ReactNode> = {
    done: <CheckCircle fontSize="small" color="success" />,
    busy: <CircularProgress size={18} />,
    todo: <Box sx={{ width: 20 }} />,
    failed: <Cancel fontSize="small" color="error" />,
  };
  return (
    <Stack direction="row" spacing={1.25} sx={{ alignItems: 'center' }}>
      {icon[state]}
      <Typography color={state === 'todo' ? 'text.secondary' : undefined}>{label}</Typography>
    </Stack>
  );
}
