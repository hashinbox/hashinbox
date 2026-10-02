import Add from '@mui/icons-material/Add';
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
import MenuItem from '@mui/material/MenuItem';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Tooltip from '@mui/material/Tooltip';
import Typography from '@mui/material/Typography';
import { useState, type SubmitEvent } from 'react';
import { call, client, type Wallet } from '@/api/client';
import { Coin } from '@/_components/coin';
import type { Shell } from '@/layout';
import { mono } from '@/theme';

interface Props {
  shell: Shell;
  wallets: Wallet[];
  selected: string | null;
  onSelect: (id: string) => void;
  onAdded: (wallet: Wallet) => void;
}

/** The address with its middle elided, so both ends stay checkable. */
function middle(text: string): string {
  return text.length > 22 ? `${text.slice(0, 10)}…${text.slice(-8)}` : text;
}

export function Rail({ shell, wallets, selected, onSelect, onAdded }: Props) {
  const [creating, setCreating] = useState(false);
  const [filtering, setFiltering] = useState(false);
  const [query, setQuery] = useState('');
  const [asset, setAsset] = useState<string | null>(null);

  const needle = query.trim().toLowerCase();
  const shown = wallets.filter(
    (w) =>
      (asset === null || w.asset === asset) &&
      (needle === '' || w.address.toLowerCase().includes(needle) || w.id.includes(needle)),
  );
  const assets = [...new Set(wallets.map((w) => w.asset))].sort();

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
          <IconButton size="small" aria-label="New wallet" onClick={() => setCreating(true)}>
            <Add fontSize="small" />
          </IconButton>
        </Tooltip>
      </Stack>

      <Collapse in={filtering}>
        <Stack spacing={1.25} sx={{ pb: 2 }}>
          <TextField
            placeholder="address or id"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            slotProps={{
              htmlInput: { 'aria-label': 'Filter by address or id', spellCheck: false },
            }}
          />
          <Stack direction="row" sx={{ flexWrap: 'wrap', gap: 0.75 }}>
            {assets.map((a) => (
              <Chip
                key={a}
                size="small"
                label={a.toUpperCase()}
                variant={asset === a ? 'filled' : 'outlined'}
                onClick={() => setAsset(asset === a ? null : a)}
                icon={<Coin symbol={a} size={16} />}
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
            <Coin symbol={w.asset} />
            <Box sx={{ minWidth: 0, flexGrow: 1 }}>
              <Typography noWrap sx={{ fontFamily: mono, fontSize: 12.5 }}>
                {middle(w.address)}
              </Typography>
              <Typography noWrap sx={{ fontSize: 11, color: 'text.secondary' }}>
                {w.asset.toUpperCase()} · {w.network.chain}/{w.network.name}
                {w.watch_only && ' · watch only'}
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
          shell.notify('Wallet created. Its address is watched from now on.');
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

function AssetField({
  shell,
  value,
  onChange,
}: {
  shell: Shell;
  value: string;
  onChange: (asset: string) => void;
}) {
  return (
    <TextField
      select
      label="Asset"
      value={value}
      onChange={(e) => onChange(e.target.value)}
      required
    >
      {shell.listings.map((l) => (
        <MenuItem key={`${l.asset}:${l.network.chain}:${l.network.name}`} value={l.asset}>
          {l.asset} · {l.network.chain}/{l.network.name}
        </MenuItem>
      ))}
    </TextField>
  );
}

function CreateDialog({ shell, open, onClose, onDone }: DialogProps) {
  const [asset, setAsset] = useState('');
  const [busy, setBusy] = useState(false);
  const chosen = asset || (shell.listings[0]?.asset ?? '');
  return (
    <Dialog open={open} onClose={onClose} fullWidth maxWidth="xs">
      <Box
        component="form"
        onSubmit={(event: SubmitEvent<HTMLFormElement>) => {
          event.preventDefault();
          setBusy(true);
          call(client.POST('/v1/wallets', { body: { asset: chosen } }))
            .then(onDone)
            .catch(shell.fail)
            .finally(() => setBusy(false));
        }}
      >
        <DialogTitle variant="h3">New wallet</DialogTitle>
        <DialogContent>
          <DialogContentText sx={{ fontSize: 13, mb: 2 }}>
            Generates a key, keeps it on this server, and watches the address from now.
          </DialogContentText>
          <AssetField shell={shell} value={chosen} onChange={setAsset} />
        </DialogContent>
        <DialogActions>
          <Button color="inherit" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="contained" loading={busy}>
            Create
          </Button>
        </DialogActions>
      </Box>
    </Dialog>
  );
}
