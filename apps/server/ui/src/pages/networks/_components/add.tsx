import Alert from '@mui/material/Alert';
import Close from '@mui/icons-material/Close';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Checkbox from '@mui/material/Checkbox';
import FormControlLabel from '@mui/material/FormControlLabel';
import Dialog from '@mui/material/Dialog';
import DialogActions from '@mui/material/DialogActions';
import DialogContent from '@mui/material/DialogContent';
import DialogTitle from '@mui/material/DialogTitle';
import Link from '@mui/material/Link';
import MenuItem from '@mui/material/MenuItem';
import IconButton from '@mui/material/IconButton';
import Stack from '@mui/material/Stack';
import Switch from '@mui/material/Switch';
import Tab from '@mui/material/Tab';
import Tabs from '@mui/material/Tabs';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useState, type SubmitEvent } from 'react';
import { ApiError, call, client, type ConfiguredNetwork, type Schemas } from '@/api/client';
import type { Shell } from '@/layout';
import { NetworkSelect } from '@/_components/network-select';
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

/** What the server said, shown in the dialog; anything else is the shell's. */
function failed(shell: Shell, setProblem: (problem: string) => void) {
  return (error: unknown) => {
    if (error instanceof ApiError && error.status !== 401) setProblem(error.message);
    else shell.fail(error);
  };
}

/**
 * Adds nodes, two ways: a provider's, one per network ticked (to enable many
 * networks fast), or one's own node, to a network or to a new one.
 */
export function AddNodes({
  shell,
  networks,
  chains,
  open,
  onClose,
  onAdded,
  onCreated,
}: {
  shell: Shell;
  networks: ConfiguredNetwork[];
  /** Every chain the server runs: what a new network may be. */
  chains: string[];
  open: boolean;
  onClose: () => void;
  onAdded: (changed: Schemas['NetworkEndpoints'][]) => void;
  /** A network was created (its node may still be refused). */
  onCreated: () => void;
}) {
  const [tab, setTab] = useState(0);
  return (
    <Dialog open={open} onClose={onClose} fullWidth maxWidth="md">
      <DialogTitle variant="h3">Add nodes</DialogTitle>
      <Tabs value={tab} onChange={(_, next: number) => setTab(next)} sx={{ px: 3 }}>
        <Tab label="Provider" />
        <Tab label="Own node" />
      </Tabs>
      {tab === 0 ? (
        <FromProvider shell={shell} networks={networks} onClose={onClose} onAdded={onAdded} />
      ) : (
        <OwnNode
          shell={shell}
          networks={networks}
          chains={chains}
          onClose={onClose}
          onAdded={onAdded}
          onCreated={onCreated}
        />
      )}
    </Dialog>
  );
}

/**
 * A provider's nodes: pick it, give what it asks for (its API key, or a URL
 * per network for providers that issue one per endpoint), keep the networks
 * it should serve (all by default), and every node is added at once.
 */
function FromProvider({
  shell,
  networks,
  onClose,
  onAdded,
}: {
  shell: Shell;
  networks: ConfiguredNetwork[];
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
      .then((answer) => onAdded(answer.networks))
      .catch(failed(shell, setProblem))
      .finally(() => setBusy(false));
  };

  return (
    <>
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
        <Button color="inherit" onClick={onClose}>
          Cancel
        </Button>
        <Button variant="contained" loading={busy} disabled={nodes.length === 0} onClick={submit}>
          Add {nodes.length === 1 ? 'node' : `${String(nodes.length)} nodes`}
        </Button>
      </DialogActions>
    </>
  );
}

/** The `NetworkSelect` value of "New network…". */
const NEW = '+new';

/** What a new network of a chain starts with: the seeded networks' values. */
interface ChainDefaults {
  /** What its `chain_id` is called; null: the name identifies the network. */
  id: string | null;
  hint: string;
  blockTimeMs: number;
  confirmations: number;
  ticker: string;
  decimals: number;
}

const DEFAULTS: Record<string, ChainDefaults> = {
  bitcoin: {
    id: null,
    hint: 'The name is the network: mainnet, testnet3, testnet4, signet or regtest.',
    blockTimeMs: 600_000,
    confirmations: 6,
    ticker: 'BTC',
    decimals: 8,
  },
  ethereum: {
    id: 'Chain ID',
    hint: 'The EIP-155 chain ID the node answers to eth_chainId, in decimal.',
    blockTimeMs: 12_000,
    confirmations: 64,
    ticker: 'ETH',
    decimals: 18,
  },
  bnb: {
    id: 'Chain ID',
    hint: 'The EIP-155 chain ID the node answers to eth_chainId, in decimal.',
    blockTimeMs: 450,
    confirmations: 15,
    ticker: 'BNB',
    decimals: 18,
  },
  solana: {
    id: 'Genesis hash',
    hint: 'What the node answers to getGenesisHash.',
    blockTimeMs: 400,
    confirmations: 32,
    ticker: 'SOL',
    decimals: 9,
  },
  tron: {
    id: 'Genesis block ID',
    hint: 'The block ID of block 0, in hex.',
    blockTimeMs: 3000,
    confirmations: 19,
    ticker: 'TRX',
    decimals: 6,
  },
};

const FALLBACK: ChainDefaults = {
  id: 'Chain ID',
  hint: 'What the node reports as its chain.',
  blockTimeMs: 1000,
  confirmations: 1,
  ticker: '',
  decimals: 18,
};

/** `text` as an http(s) URL; null otherwise. */
function httpUrl(text: string): URL | null {
  try {
    const url = new URL(text.trim());
    return url.protocol === 'http:' || url.protocol === 'https:' ? url : null;
  } catch {
    return null;
  }
}

/**
 * One's own node: its RPC URL and headers, tested against the network
 * before it is added; the network picked, or a new one created first.
 */
function OwnNode({
  shell,
  networks,
  chains,
  onClose,
  onAdded,
  onCreated,
}: {
  shell: Shell;
  networks: ConfiguredNetwork[];
  chains: string[];
  onClose: () => void;
  onAdded: (changed: Schemas['NetworkEndpoints'][]) => void;
  onCreated: () => void;
}) {
  const [picked, setPicked] = useState(networks[0] ? name(networks[0]) : NEW);
  const [url, setUrl] = useState('');
  const [headers, setHeaders] = useState<{ name: string; value: string }[]>([]);
  const [chain, setChain] = useState(chains[0] ?? '');
  const [network, setNetwork] = useState('');
  const [chainId, setChainId] = useState('');
  const [testnet, setTestnet] = useState(true);
  const [blockTime, setBlockTime] = useState('');
  const [confirmations, setConfirmations] = useState('');
  const [ticker, setTicker] = useState('');
  const [decimals, setDecimals] = useState('');
  const [problem, setProblem] = useState<string | null>(null);
  const [told, setTold] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const creating = picked === NEW;
  const defaults = DEFAULTS[chain] ?? FALLBACK;
  const [atChain = '', atNetwork = ''] = creating ? [chain, network.trim()] : picked.split('/');
  const parsed = httpUrl(url);
  /** The URL as stored: each header a `header.<Name>` query parameter. */
  const endpoint = (() => {
    if (!parsed) return null;
    const out = new URL(parsed);
    for (const h of headers)
      if (h.name.trim() !== '') out.searchParams.append(`header.${h.name.trim()}`, h.value);
    return out.toString();
  })();
  const number = (text: string, fallback: number) => (text.trim() === '' ? fallback : Number(text));
  const fresh = {
    chain,
    network: atNetwork,
    chain_id: defaults.id === null ? null : typed(chainId),
    testnet,
    block_time_ms: number(blockTime, defaults.blockTimeMs),
    confirmations: number(confirmations, defaults.confirmations),
    native: {
      ticker: typed(ticker) ?? defaults.ticker,
      decimals: number(decimals, defaults.decimals),
    },
  };
  const complete =
    endpoint !== null &&
    (!creating ||
      (chain !== '' &&
        atNetwork !== '' &&
        (defaults.id === null || fresh.chain_id !== null) &&
        fresh.native.ticker !== '' &&
        [fresh.block_time_ms, fresh.confirmations, fresh.native.decimals].every(
          (n) => Number.isInteger(n) && n >= 0,
        )));

  const path = { chain: atChain, network: atNetwork };

  const test = () => {
    if (!endpoint) return;
    setBusy(true);
    setProblem(null);
    setTold(null);
    const at = '/v1/networks/{chain}/{network}/endpoints/test';
    call(client.POST(at, { params: { path }, body: { url: endpoint } }))
      .then((info) => setTold(`Serves ${info.chain_id}, this network; tip #${String(info.tip)}.`))
      .catch(failed(shell, setProblem))
      .finally(() => setBusy(false));
  };

  const submit = async () => {
    if (!endpoint) return;
    setBusy(true);
    setProblem(null);
    setTold(null);
    try {
      if (creating) {
        await call(client.POST('/v1/networks', { body: fresh }));
        // Created: a refused node is retried on it, not created again.
        setPicked(`${atChain}/${atNetwork}`);
        onCreated();
      }
      const at = '/v1/networks/{chain}/{network}/endpoints';
      const added = await call(
        client.POST(at, { params: { path }, body: { endpoints: [endpoint] } }),
      );
      onAdded([added]);
    } catch (error) {
      failed(shell, setProblem)(error);
    } finally {
      setBusy(false);
    }
  };

  const small = { htmlInput: { spellCheck: false, sx: { fontFamily: mono, fontSize: 13 } } };

  return (
    <Box
      component="form"
      onSubmit={(event: SubmitEvent<HTMLFormElement>) => {
        event.preventDefault();
        void submit();
      }}
    >
      <DialogContent>
        <Stack spacing={2} sx={{ mt: 1 }}>
          <NetworkSelect
            value={picked}
            networks={networks.map((n) => ({
              value: name(n),
              ticker: n.assets.find((a) => a.kind === 'native')?.ticker ?? null,
            }))}
            extra={{ value: NEW, label: 'New network…' }}
            onChange={(next) => {
              setPicked(next);
              setTold(null);
            }}
          />
          {creating && (
            <Stack spacing={2} sx={{ pl: 2, borderLeft: 2, borderColor: 'divider' }}>
              <Stack direction={{ xs: 'column', sm: 'row' }} spacing={2}>
                <TextField
                  select
                  label="Chain"
                  value={chain}
                  onChange={(e) => {
                    setChain(e.target.value);
                    setChainId('');
                  }}
                  sx={{ minWidth: 160 }}
                >
                  {chains.map((c) => (
                    <MenuItem key={c} value={c}>
                      {c}
                    </MenuItem>
                  ))}
                </TextField>
                <TextField
                  fullWidth
                  label="Network name"
                  placeholder="my-devnet"
                  value={network}
                  onChange={(e) => setNetwork(e.target.value.toLowerCase())}
                  helperText={defaults.id === null ? defaults.hint : 'a-z, 0-9 and -'}
                  slotProps={small}
                />
              </Stack>
              {defaults.id !== null && (
                <TextField
                  label={defaults.id}
                  value={chainId}
                  onChange={(e) => setChainId(e.target.value)}
                  helperText={defaults.hint}
                  slotProps={small}
                />
              )}
              <Stack direction={{ xs: 'column', sm: 'row' }} spacing={2}>
                <TextField
                  label="Block time (ms)"
                  type="number"
                  placeholder={String(defaults.blockTimeMs)}
                  value={blockTime}
                  onChange={(e) => setBlockTime(e.target.value)}
                  slotProps={{ inputLabel: { shrink: true } }}
                />
                <TextField
                  label="Confirmations"
                  type="number"
                  placeholder={String(defaults.confirmations)}
                  value={confirmations}
                  onChange={(e) => setConfirmations(e.target.value)}
                  slotProps={{ inputLabel: { shrink: true } }}
                />
                <TextField
                  label="Native ticker"
                  placeholder={defaults.ticker}
                  value={ticker}
                  onChange={(e) => setTicker(e.target.value.toUpperCase())}
                  slotProps={{ inputLabel: { shrink: true } }}
                />
                <TextField
                  label="Decimals"
                  type="number"
                  placeholder={String(defaults.decimals)}
                  value={decimals}
                  onChange={(e) => setDecimals(e.target.value)}
                  slotProps={{ inputLabel: { shrink: true } }}
                />
              </Stack>
              <FormControlLabel
                control={
                  <Switch checked={testnet} onChange={(e) => setTestnet(e.target.checked)} />
                }
                label="Testnet: its coins are worthless test coins"
              />
            </Stack>
          )}
          <TextField
            fullWidth
            label="RPC URL"
            placeholder="https://…"
            value={url}
            onChange={(e) => {
              setUrl(e.target.value);
              setTold(null);
            }}
            error={url.trim() !== '' && !parsed}
            helperText={
              url.trim() !== '' && !parsed
                ? 'An http:// or https:// URL.'
                : 'A key in the URL (path, query, user:password@) is kept as written.'
            }
            slotProps={small}
          />
          <Stack spacing={1}>
            {headers.map((h, index) => (
              <Stack key={index} direction="row" spacing={1} sx={{ alignItems: 'center' }}>
                <TextField
                  size="small"
                  label="Header"
                  placeholder="x-api-key"
                  value={h.name}
                  onChange={(e) =>
                    setHeaders((all) =>
                      all.map((one, at) => (at === index ? { ...one, name: e.target.value } : one)),
                    )
                  }
                  slotProps={small}
                />
                <TextField
                  size="small"
                  fullWidth
                  label="Value"
                  value={h.value}
                  onChange={(e) =>
                    setHeaders((all) =>
                      all.map((one, at) =>
                        at === index ? { ...one, value: e.target.value } : one,
                      ),
                    )
                  }
                  slotProps={small}
                />
                <IconButton
                  size="small"
                  aria-label="Remove header"
                  onClick={() => setHeaders((all) => all.filter((_, at) => at !== index))}
                >
                  <Close fontSize="small" />
                </IconButton>
              </Stack>
            ))}
            <Box>
              <Button
                size="small"
                color="inherit"
                onClick={() => setHeaders((all) => [...all, { name: '', value: '' }])}
              >
                Add header
              </Button>
              <Typography component="span" variant="body2" color="text.secondary" sx={{ ml: 1 }}>
                Stored in the URL as header.NAME=value, sent as that header.
              </Typography>
            </Box>
          </Stack>
          {told && <Alert severity="success">{told}</Alert>}
          {problem && <Alert severity="error">{problem}</Alert>}
        </Stack>
      </DialogContent>
      <DialogActions>
        <Button color="inherit" onClick={onClose}>
          Cancel
        </Button>
        <Button
          color="inherit"
          disabled={busy || endpoint === null || creating}
          title={creating ? 'A new network is tested once it exists: add the node.' : undefined}
          onClick={test}
        >
          Test
        </Button>
        <Button type="submit" variant="contained" loading={busy} disabled={!complete}>
          {creating ? 'Create network and add node' : 'Add node'}
        </Button>
      </DialogActions>
    </Box>
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
