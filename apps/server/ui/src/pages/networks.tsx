import Alert from '@mui/material/Alert';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Chip from '@mui/material/Chip';
import Dialog from '@mui/material/Dialog';
import DialogActions from '@mui/material/DialogActions';
import DialogContent from '@mui/material/DialogContent';
import DialogContentText from '@mui/material/DialogContentText';
import DialogTitle from '@mui/material/DialogTitle';
import Stack from '@mui/material/Stack';
import Table from '@mui/material/Table';
import TableBody from '@mui/material/TableBody';
import TableCell from '@mui/material/TableCell';
import TableContainer from '@mui/material/TableContainer';
import TableHead from '@mui/material/TableHead';
import TableRow from '@mui/material/TableRow';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useCallback, useEffect, useState, type SubmitEvent } from 'react';
import { ApiError, call, client, type ConfiguredNetwork } from '@/api/client';
import { Confirm } from '@/_components/confirm';
import { EmptyRow } from '@/_components/table';
import type { Shell } from '@/layout';
import { mono } from '@/theme';

const BEAT_MS = 4000;

type State = ConfiguredNetwork['state'];

const TONE: Record<State, 'default' | 'success' | 'info' | 'warning' | 'error'> = {
  unavailable: 'default',
  rescanning: 'info',
  paused: 'warning',
  catching_up: 'info',
  ready: 'success',
  retrying: 'warning',
  quarantined: 'error',
};

function nameOf(n: ConfiguredNetwork): string {
  return `${n.network.chain}/${n.network.name}`;
}

/** Every configured network, its state and RPC nodes, with node edits and rescans. */
export function Networks({ shell }: { shell: Shell }) {
  const [networks, setNetworks] = useState<ConfiguredNetwork[]>([]);
  const [editing, setEditing] = useState<ConfiguredNetwork | null>(null);
  const [rescanning, setRescanning] = useState<ConfiguredNetwork | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const refresh = useCallback(() => {
    call(client.GET('/v1/networks'))
      .then((list) => setNetworks(list.networks))
      .catch(shell.fail);
  }, [shell.fail]);

  useEffect(() => {
    refresh();
    const timer = window.setInterval(refresh, BEAT_MS);
    return () => window.clearInterval(timer);
  }, [refresh]);

  const rescan = (target: ConfiguredNetwork) => {
    setRescanning(null);
    setBusy(nameOf(target));
    const params = { path: { chain: target.network.chain, network: target.network.name } };
    call(client.POST('/v1/networks/{chain}/{network}/rescans', { params }))
      .then(() => {
        shell.notify('Rescan started; indexing again from birthdays.');
        refresh();
      })
      .catch(shell.fail)
      .finally(() => setBusy(null));
  };

  return (
    <Box sx={{ px: { xs: 2, md: 4 }, py: { xs: 2.5, md: 3.5 }, maxWidth: 1100 }}>
      <Typography variant="h2" component="h1">
        Networks
      </Typography>
      <Typography variant="body2" color="text.secondary" sx={{ mt: 1, mb: 3, maxWidth: 720 }}>
        The configuration file&apos;s nodes only seed the database on the first start; after that
        the stored list is used. An edit is verified, then applied live and stored. Only each
        node&apos;s origin is shown.
      </Typography>
      <TableContainer>
        <Table>
          <TableHead>
            <TableRow>
              <TableCell>Network</TableCell>
              <TableCell>State</TableCell>
              <TableCell>Nodes, in failover order</TableCell>
              <TableCell align="right" />
            </TableRow>
          </TableHead>
          <TableBody>
            {networks.length === 0 && <EmptyRow columns={4} message="No network is configured." />}
            {networks.map((n) => {
              const name = nameOf(n);
              return (
                <TableRow key={name}>
                  <TableCell sx={{ fontFamily: mono }}>{name}</TableCell>
                  <TableCell>
                    <Chip size="small" label={n.state} color={TONE[n.state]} />
                  </TableCell>
                  <TableCell>
                    {n.nodes.map((node, i) => (
                      <Typography key={i} sx={{ fontFamily: mono, fontSize: 12.5 }}>
                        {node.origin}
                      </Typography>
                    ))}
                  </TableCell>
                  <TableCell align="right">
                    <Stack direction="row" spacing={1} sx={{ justifyContent: 'flex-end' }}>
                      <Button size="small" variant="outlined" onClick={() => setEditing(n)}>
                        Edit RPC
                      </Button>
                      <Button
                        size="small"
                        variant="outlined"
                        color="error"
                        loading={busy === name}
                        disabled={n.state === 'rescanning' || n.state === 'unavailable'}
                        onClick={() => setRescanning(n)}
                      >
                        Rescan
                      </Button>
                    </Stack>
                  </TableCell>
                </TableRow>
              );
            })}
          </TableBody>
        </Table>
      </TableContainer>

      <Nodes
        shell={shell}
        network={editing}
        onClose={() => setEditing(null)}
        onSaved={(saved) => {
          setEditing(null);
          shell.notify(`${nameOf(saved)} now uses the new nodes.`);
          refresh();
        }}
      />
      <Confirm
        open={rescanning !== null}
        title={`Rescan ${rescanning ? nameOf(rescanning) : ''}`}
        phrase={rescanning ? nameOf(rescanning) : ''}
        action="Rescan"
        onClose={() => setRescanning(null)}
        onConfirm={() => {
          if (rescanning) rescan(rescanning);
        }}
      >
        Deletes this network&apos;s indexed history and balances and indexes again from every
        wallet&apos;s birthday. Wallets and keys are kept. New wallets and sends on this network are
        refused while it runs.
      </Confirm>
    </Box>
  );
}

const MAX_NODES = 16;

interface NodesProps {
  shell: Shell;
  /** The network being edited; null keeps the dialog closed. */
  network: ConfiguredNetwork | null;
  onClose: () => void;
  onSaved: (network: ConfiguredNetwork) => void;
}

/** One URL per line, blanks dropped, in the order typed: failover order. */
function urls(text: string): string[] {
  return text
    .split('\n')
    .map((line) => line.trim())
    .filter((line) => line !== '');
}

/**
 * Replaces a network's RPC nodes. Only origins ever come back from the server,
 * so the field starts empty: full URLs (with any credentials) are typed anew.
 */
function Nodes({ shell, network, onClose, onSaved }: NodesProps) {
  const [text, setText] = useState('');
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const close = () => {
    setText('');
    setProblem(null);
    onClose();
  };

  const submit = (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (!network) return;
    const endpoints = urls(text);
    if (endpoints.length === 0 || endpoints.length > MAX_NODES) {
      setProblem(`List between 1 and ${MAX_NODES} nodes.`);
      return;
    }
    setBusy(true);
    setProblem(null);
    const params = { path: { chain: network.network.chain, network: network.network.name } };
    call(client.PUT('/v1/networks/{chain}/{network}/rpc', { params, body: { endpoints } }))
      .then((saved) => {
        setText('');
        onSaved(saved);
      })
      .catch((error: unknown) => {
        // The dialog stays open: the current nodes are still in use.
        if (error instanceof ApiError && error.status !== 401) setProblem(error.message);
        else shell.fail(error);
      })
      .finally(() => setBusy(false));
  };

  const name = network ? `${network.network.chain}/${network.network.name}` : '';
  const hint = network?.nodes.map((n) => n.origin).join('\n') ?? '';

  return (
    <Dialog open={network !== null} onClose={close} fullWidth maxWidth="sm">
      <Box component="form" autoComplete="off" onSubmit={submit}>
        <DialogTitle variant="h3">RPC nodes · {name}</DialogTitle>
        <DialogContent>
          <DialogContentText component="div" sx={{ fontSize: 13, mb: 2 }}>
            One full URL per line, in failover order. Each is verified first, then used at once and
            stored; the stored list replaces the configuration file&apos;s from now on. Credentials
            are never shown again, so type them out, or use{' '}
            <Box component="span" sx={{ fontFamily: mono }}>
              {'${NAME}'}
            </Box>{' '}
            for a server environment variable.
          </DialogContentText>
          <TextField
            label="URLs"
            placeholder={hint}
            value={text}
            onChange={(e) => setText(e.target.value)}
            multiline
            minRows={4}
            fullWidth
            slotProps={{ htmlInput: { spellCheck: false, sx: { fontFamily: mono, fontSize: 13 } } }}
            autoFocus
            required
          />
          {problem && (
            <Alert severity="error" sx={{ mt: 2 }}>
              {problem}
            </Alert>
          )}
        </DialogContent>
        <DialogActions>
          <Button color="inherit" onClick={close}>
            Cancel
          </Button>
          <Button type="submit" variant="contained" loading={busy}>
            Verify and use
          </Button>
        </DialogActions>
      </Box>
    </Dialog>
  );
}
