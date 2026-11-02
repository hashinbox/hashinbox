import Add from '@mui/icons-material/Add';
import Autocomplete from '@mui/material/Autocomplete';
import FilterList from '@mui/icons-material/FilterList';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Chip from '@mui/material/Chip';
import Collapse from '@mui/material/Collapse';
import Dialog from '@mui/material/Dialog';
import DialogActions from '@mui/material/DialogActions';
import DialogContent from '@mui/material/DialogContent';
import DialogContentText from '@mui/material/DialogContentText';
import DialogTitle from '@mui/material/DialogTitle';
import IconButton from '@mui/material/IconButton';
import List from '@mui/material/List';
import ListItemButton from '@mui/material/ListItemButton';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Tooltip from '@mui/material/Tooltip';
import Typography from '@mui/material/Typography';
import FormControlLabel from '@mui/material/FormControlLabel';
import Switch from '@mui/material/Switch';
import ToggleButton from '@mui/material/ToggleButton';
import ToggleButtonGroup from '@mui/material/ToggleButtonGroup';
import { useState, type SubmitEvent } from 'react';
import { call, client, type Wallet } from '@/api/client';
import { Currency } from '@/_components/currency';
import type { Shell } from '@/layout';
import { networkChoices, unitOf } from '@/money';
import { mono } from '@/theme';
import { WatchFields } from './watch';
import { WATCH_NOTHING, watchBody, type Watching } from './watching';
import { NetworkSelect } from '@/_components/network-select';

interface Props {
  shell: Shell;
  wallets: Wallet[];
  selected: string | null;
  onSelect: (id: string) => void;
  onAdded: (wallet: Wallet) => void;
  /** Tags by wallet id. */
  tags: Map<string, string[]>;
}

/** The address with its middle elided, so both ends stay checkable. */
function middle(text: string): string {
  return text.length > 22 ? `${text.slice(0, 10)}…${text.slice(-8)}` : text;
}

export function Rail({ shell, wallets, selected, onSelect, onAdded, tags }: Props) {
  const [creating, setCreating] = useState(false);
  const [filtering, setFiltering] = useState(false);
  const [query, setQuery] = useState('');
  const [asset, setAsset] = useState<string | null>(null);

  const needle = query.trim().toLowerCase();
  const shown = wallets.filter(
    (w) =>
      (asset === null || unitOf(w, shell.listings) === asset) &&
      (needle === '' ||
        w.address.toLowerCase().includes(needle) ||
        w.id.includes(needle) ||
        (tags.get(w.id) ?? []).some((t) => t.includes(needle))),
  );
  // Filtered by currency (ETH, USDC…), whatever network it is on.
  const assets = [...new Set(wallets.map((w) => unitOf(w, shell.listings)))].sort();

  return (
    <Box
      component="aside"
      sx={(t) => ({
        p: 2.5,
        minWidth: 0,
        // Each side named in full: a responsive border shorthand would reset its colour.
        borderBottom: `1px solid ${t.palette.divider}`,
        [t.breakpoints.up('md')]: {
          borderBottom: 0,
          borderRight: `1px solid ${t.palette.divider}`,
        },
      })}
    >
      <Stack direction="row" sx={{ alignItems: 'center', mb: 1.5 }}>
        <Typography variant="h3" sx={{ flexGrow: 1 }}>
          Wallets{' '}
          <Typography
            component="span"
            color="text.secondary"
            sx={{ fontFamily: mono, fontSize: 12 }}
          >
            {shown.length === wallets.length ? wallets.length : `${shown.length}/${wallets.length}`}
          </Typography>
        </Typography>
        <Tooltip title="Filter">
          <IconButton
            size="small"
            aria-label="Filter wallets"
            aria-pressed={filtering}
            onClick={() => setFiltering(!filtering)}
            sx={{ color: filtering ? 'text.primary' : 'text.secondary' }}
          >
            <FilterList fontSize="small" />
          </IconButton>
        </Tooltip>
        <Tooltip title="New wallet">
          <IconButton
            size="small"
            aria-label="New wallet"
            onClick={(e) => {
              e.currentTarget.blur();
              setCreating(true);
            }}
          >
            <Add fontSize="small" />
          </IconButton>
        </Tooltip>
      </Stack>

      <Collapse in={filtering}>
        <Stack spacing={1.25} sx={{ pb: 2 }}>
          <TextField
            placeholder="tag, address or id"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            slotProps={{
              htmlInput: { 'aria-label': 'Filter by tag, address or id', spellCheck: false },
            }}
          />
          <Stack direction="row" sx={{ flexWrap: 'wrap', gap: 0.75 }}>
            {assets.map((a) => (
              <Chip
                key={a}
                size="small"
                label={<Currency ticker={a} />}
                variant={asset === a ? 'filled' : 'outlined'}
                onClick={() => setAsset(asset === a ? null : a)}
              />
            ))}
          </Stack>
        </Stack>
      </Collapse>

      <List disablePadding aria-label="Wallets" sx={{ borderBottom: 1, borderColor: 'divider' }}>
        {shown.map((w) => (
          <ListItemButton
            key={w.id}
            selected={w.id === selected}
            onClick={() => onSelect(w.id)}
            sx={{ gap: 1.5, py: 1.25, px: 1 }}
          >
            <Box sx={{ display: 'flex', fontSize: 22, flex: 'none' }}>
              <Currency ticker={unitOf(w, shell.listings)} label={false} />
            </Box>
            <Box sx={{ minWidth: 0, flexGrow: 1 }}>
              <Typography noWrap sx={{ fontFamily: mono, fontSize: 12.5 }}>
                {middle(w.address)}
              </Typography>
              <Typography noWrap sx={{ fontSize: 11, color: 'text.secondary' }}>
                {unitOf(w, shell.listings)} · {w.network.chain}/{w.network.name}
                {!w.keyed && ' · watch only'}
                {(tags.get(w.id) ?? []).map((t) => ` · #${t}`).join('')}
              </Typography>
            </Box>
          </ListItemButton>
        ))}
        {shown.length === 0 && (
          <Typography
            color="text.secondary"
            sx={{ py: 2, fontSize: 13, borderTop: 1, borderColor: 'divider' }}
          >
            {wallets.length === 0 ? 'No wallets yet.' : 'Nothing matches.'}
          </Typography>
        )}
      </List>

      <CreateDialog
        shell={shell}
        open={creating}
        onClose={() => setCreating(false)}
        onDone={(wallet) => {
          setCreating(false);
          onAdded(wallet);
          shell.notify('Wallet added.');
        }}
      />
    </Box>
  );
}

interface DialogProps {
  shell: Shell;
  open: boolean;
  onClose: () => void;
  onDone: (wallet: Wallet) => void;
}

/** The networks the configured assets live in, as `chain/network`. */
function networksOf(shell: Shell): string[] {
  return [...new Set(shell.listings.map((l) => `${l.network.chain}/${l.network.name}`))];
}

function CreateDialog({ shell, open, onClose, onDone }: DialogProps) {
  const [mode, setMode] = useState<'generate' | 'watch'>('generate');
  const [network, setNetwork] = useState('');
  const [address, setAddress] = useState('');
  const [watchNow, setWatchNow] = useState(true);
  const [watching, setWatching] = useState<Watching>({ ...WATCH_NOTHING, assets: ['native'] });
  const [tags, setTags] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const chosen = network || (networksOf(shell)[0] ?? '');
  const [chain = '', name = ''] = chosen.split('/');
  const offered = shell.listings.filter(
    (l) => l.network.chain === chain && l.network.name === name,
  );
  const watches = mode === 'watch' || watchNow;
  const ready =
    (mode === 'generate' || address.trim() !== '') && (!watches || watching.assets.length > 0);

  const reset = () => {
    setAddress('');
    setWatching({ ...WATCH_NOTHING, assets: ['native'] });
    setTags([]);
  };

  return (
    <Dialog open={open} onClose={onClose} fullWidth maxWidth="xs">
      <Box
        component="form"
        onSubmit={(event: SubmitEvent<HTMLFormElement>) => {
          event.preventDefault();
          setBusy(true);
          const body = {
            chain,
            network: name,
            ...(mode === 'watch' ? { address: address.trim() } : {}),
            ...(watches ? watchBody(watching) : { assets: [] }),
          };
          call(client.POST('/v1/wallets', { body }))
            .then(async (wallet) => {
              for (const tag of tags) {
                const params = { path: { id: wallet.id, tag } };
                await call(client.PUT('/v1/wallets/{id}/tags/{tag}', { params }));
              }
              reset();
              onDone(wallet);
            })
            .catch(shell.fail)
            .finally(() => setBusy(false));
        }}
      >
        <DialogTitle variant="h3">New wallet</DialogTitle>
        <DialogContent>
          <Stack spacing={2} sx={{ mt: 1 }}>
            <ToggleButtonGroup
              exclusive
              fullWidth
              size="small"
              value={mode}
              onChange={(_, next: 'generate' | 'watch' | null) => next && setMode(next)}
            >
              <ToggleButton value="generate">Generate a wallet</ToggleButton>
              <ToggleButton value="watch">Add an address</ToggleButton>
            </ToggleButtonGroup>
            <DialogContentText sx={{ fontSize: 13 }}>
              {mode === 'generate'
                ? 'Generates a key and keeps it on this server.'
                : 'Watches an existing address; its key is not kept here, so it cannot send.'}
            </DialogContentText>
            <NetworkSelect
              value={chosen}
              networks={networkChoices(shell.listings)}
              onChange={(value) => {
                setNetwork(value);
                setWatching({ ...watching, assets: ['native'] });
              }}
              required
            />
            {mode === 'watch' && (
              <TextField
                label="Address"
                value={address}
                onChange={(e) => setAddress(e.target.value)}
                slotProps={{ htmlInput: { spellCheck: false } }}
                required
              />
            )}
            {mode === 'generate' && (
              <FormControlLabel
                control={
                  <Switch checked={watchNow} onChange={(e) => setWatchNow(e.target.checked)} />
                }
                label={watchNow ? 'Watch it now' : 'Do not watch yet (watch it later)'}
              />
            )}
            {watches && <WatchFields offered={offered} value={watching} onChange={setWatching} />}
            <Autocomplete
              multiple
              freeSolo
              options={[]}
              value={tags}
              onChange={(_, next) =>
                setTags(next.map((t) => t.trim().toLowerCase()).filter((t) => t !== ''))
              }
              renderInput={(params) => (
                <TextField {...params} label="Tags (optional)" placeholder="deposit" />
              )}
            />
          </Stack>
        </DialogContent>
        <DialogActions>
          <Button color="inherit" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="contained" loading={busy} disabled={!ready}>
            {mode === 'generate' ? 'Generate' : 'Add'}
          </Button>
        </DialogActions>
      </Box>
    </Dialog>
  );
}
