import Alert from '@mui/material/Alert';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Checkbox from '@mui/material/Checkbox';
import Dialog from '@mui/material/Dialog';
import DialogActions from '@mui/material/DialogActions';
import DialogContent from '@mui/material/DialogContent';
import DialogTitle from '@mui/material/DialogTitle';
import Link from '@mui/material/Link';
import MenuItem from '@mui/material/MenuItem';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useState, type SubmitEvent } from 'react';
import { ApiError, call, client, type ConfiguredNetwork, type Schemas } from '@/api/client';
import type { Shell } from '@/layout';
import { PROVIDERS, asksKey, baseFor, urlOf } from '@/providers';
import { mono } from '@/theme';

/** `text` trimmed; null when empty or absent. */
function typed(text: string | undefined): string | null {
  const trimmed = text?.trim() ?? '';
  return trimmed === '' ? null : trimmed;
}

function name(n: ConfiguredNetwork): string {
  return `${n.chain}/${n.network}`;
}

/**
 * Adds a provider's nodes: pick it, give what it asks for (its API key, or a
 * URL per network for providers that issue one per endpoint), keep the
 * networks it should serve (all by default), and every node is added at once.
 */
export function AddNodes({
  shell,
  networks,
  open,
  onClose,
  onAdded,
}: {
  shell: Shell;
  networks: ConfiguredNetwork[];
  open: boolean;
  onClose: () => void;
  onAdded: (changed: Schemas['NetworkEndpoints'][]) => void;
}) {
  const offered = PROVIDERS.filter((p) =>
    networks.some((n) => baseFor(p, n.chain, n.network) !== undefined),
  );
  const [providerName, setProvider] = useState('');
  const [key, setKey] = useState('');
  const [urls, setUrls] = useState<Record<string, string>>({});
  const [skipped, setSkipped] = useState<Set<string>>(new Set());
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const provider = offered.find((p) => p.name === providerName) ?? offered[0];
  const served = provider
    ? networks.flatMap((n) => {
        const base = baseFor(provider, n.chain, n.network);
        return base === undefined ? [] : [{ network: n, base }];
      })
    : [];
  const perNetwork = provider?.auth.in === 'endpoint';
  /** The URL `network` gets; null while what it needs is missing. */
  const urlFor = (network: ConfiguredNetwork, base: string | null) =>
    perNetwork || !provider || base === null
      ? typed(urls[name(network)])
      : urlOf(provider, base, key);
  const nodes = served
    .filter(({ network }) => !skipped.has(name(network)))
    .flatMap(({ network, base }) => {
      const url = urlFor(network, base);
      return url ? [{ network, url }] : [];
    });

  const close = () => {
    setKey('');
    setUrls({});
    setSkipped(new Set());
    setProblem(null);
    onClose();
  };

  const submit = () => {
    const body = {
      nodes: nodes.map(({ network, url }) => ({
        chain: network.chain,
        network: network.network,
        endpoint: url,
      })),
    };
    setBusy(true);
    setProblem(null);
    call(client.POST('/v1/networks/endpoints', { body }))
      .then((answer) => {
        close();
        onAdded(answer.networks);
      })
      .catch((error: unknown) => {
        if (error instanceof ApiError && error.status !== 401) setProblem(error.message);
        else shell.fail(error);
      })
      .finally(() => setBusy(false));
  };

  return (
    <Dialog open={open} onClose={close} fullWidth maxWidth="md">
      <DialogTitle variant="h3">Add a provider</DialogTitle>
      <DialogContent>
        {!provider ? (
          <Alert severity="info">No known provider serves the configured networks.</Alert>
        ) : (
          <Stack spacing={2} sx={{ mt: 1 }}>
            <TextField
              select
              label="Provider"
              value={provider.name}
              onChange={(e) => {
                setProvider(e.target.value);
                setKey('');
                setUrls({});
                setSkipped(new Set());
              }}
              helperText={
                <Link href={provider.docs} target="_blank" rel="noreferrer">
                  {provider.name} docs
                </Link>
              }
            >
              {offered.map((p) => (
                <MenuItem key={p.name} value={p.name}>
                  {p.name}
                </MenuItem>
              ))}
            </TextField>
            {asksKey(provider) && (
              <TextField
                label={
                  provider.auth.in === 'header' || provider.auth.in === 'query'
                    ? `API key (sent as ${provider.auth.in} ${provider.auth.name})`
                    : 'API key (in the URL path)'
                }
                value={key}
                onChange={(e) => setKey(e.target.value)}
                slotProps={{ htmlInput: { spellCheck: false } }}
                autoFocus
              />
            )}
            <Typography variant="body2" color="text.secondary">
              {perNetwork
                ? 'Each endpoint has its own URL and token: paste one per network.'
                : 'Adds one node to each network ticked:'}
            </Typography>
            <Stack spacing={1}>
              {served.map(({ network, base }) => {
                const at = name(network);
                const on = !skipped.has(at);
                const url = urlFor(network, base);
                return (
                  <Stack key={at} direction="row" spacing={1} sx={{ alignItems: 'center' }}>
                    <Checkbox
                      size="small"
                      checked={on}
                      onChange={(e) =>
                        setSkipped((s) => {
                          const next = new Set(s);
                          if (e.target.checked) next.delete(at);
                          else next.add(at);
                          return next;
                        })
                      }
                    />
                    <Typography sx={{ fontFamily: mono, fontSize: 13, minWidth: 170 }}>
                      {at}
                    </Typography>
                    {perNetwork ? (
                      <TextField
                        size="small"
                        fullWidth
                        disabled={!on}
                        placeholder="https://…"
                        value={urls[at] ?? ''}
                        onChange={(e) => setUrls((u) => ({ ...u, [at]: e.target.value }))}
                        slotProps={{
                          htmlInput: {
                            spellCheck: false,
                            sx: { fontFamily: mono, fontSize: 12.5 },
                          },
                        }}
                      />
                    ) : (
                      <Typography
                        noWrap
                        title={url ?? undefined}
                        sx={{
                          fontFamily: mono,
                          fontSize: 12,
                          color: url ? 'text.primary' : 'text.secondary',
                        }}
                      >
                        {url ?? ''}
                      </Typography>
                    )}
                  </Stack>
                );
              })}
            </Stack>
            {problem && <Alert severity="error">{problem}</Alert>}
          </Stack>
        )}
      </DialogContent>
      <DialogActions>
        <Button color="inherit" onClick={close}>
          Cancel
        </Button>
        <Button variant="contained" loading={busy} disabled={nodes.length === 0} onClick={submit}>
          Add {nodes.length === 1 ? 'node' : `${String(nodes.length)} nodes`}
        </Button>
      </DialogActions>
    </Dialog>
  );
}

/**
 * Adds one node to `network`: its RPC URL. A provider is a shortcut that
 * writes the URL from its keys; the URL can be edited after.
 */
export function AddNode({
  shell,
  network,
  open,
  onClose,
  onAdded,
}: {
  shell: Shell;
  network: ConfiguredNetwork;
  open: boolean;
  onClose: () => void;
  onAdded: () => void;
}) {
  const [url, setUrl] = useState('');
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const close = () => {
    setUrl('');
    setProblem(null);
    onClose();
  };

  const submit = () => {
    const body = {
      nodes: [{ chain: network.chain, network: network.network, endpoint: url.trim() }],
    };
    setBusy(true);
    setProblem(null);
    call(client.POST('/v1/networks/endpoints', { body }))
      .then(() => {
        close();
        onAdded();
      })
      .catch((error: unknown) => {
        if (error instanceof ApiError && error.status !== 401) setProblem(error.message);
        else shell.fail(error);
      })
      .finally(() => setBusy(false));
  };

  return (
    <Dialog open={open} onClose={close} fullWidth maxWidth="sm">
      <Box
        component="form"
        onSubmit={(event: SubmitEvent<HTMLFormElement>) => {
          event.preventDefault();
          submit();
        }}
      >
        <DialogTitle variant="h3">Add node · {name(network)}</DialogTitle>
        <DialogContent>
          <TextField
            fullWidth
            label="RPC URL"
            placeholder="https://…"
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            helperText="Added after the current nodes and used at once. A header.NAME=value query parameter is sent as that header."
            slotProps={{ htmlInput: { spellCheck: false, sx: { fontFamily: mono, fontSize: 13 } } }}
            sx={{ mt: 1 }}
            autoFocus
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
          <Button type="submit" variant="contained" loading={busy} disabled={url.trim() === ''}>
            Add node
          </Button>
        </DialogActions>
      </Box>
    </Dialog>
  );
}
