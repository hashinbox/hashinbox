import Dialog from '@mui/material/Dialog';
import DialogContent from '@mui/material/DialogContent';
import DialogTitle from '@mui/material/DialogTitle';
import ListItemButton from '@mui/material/ListItemButton';
import MenuItem from '@mui/material/MenuItem';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useState } from 'react';
import type { Shell } from '@/layout';
import { TEMPLATES, blank, type Definition } from './forms';

/** How new triggers start: blank, or from a template, on one network. */
export function Start({
  shell,
  open,
  onClose,
  onStart,
}: {
  shell: Shell;
  open: boolean;
  onClose: () => void;
  onStart: (title: string, drafts: Definition[]) => void;
}) {
  const networks = [...new Set(shell.listings.map((l) => `${l.network.chain}/${l.network.name}`))];
  const [network, setNetwork] = useState('');
  const chosen = network || (networks[0] ?? '');
  const [chain = '', name = ''] = chosen.split('/');
  const token =
    shell.listings.find(
      (l) => l.network.chain === chain && l.network.name === name && l.asset !== 'native',
    )?.asset ?? '';

  const option = (title: string, description: string, make: () => Definition[], off = false) => (
    <ListItemButton
      key={title}
      disabled={off}
      onClick={() => onStart(title, make())}
      sx={{ display: 'block', borderRadius: 1 }}
    >
      <Typography sx={{ fontWeight: 600, fontSize: 14 }}>{title}</Typography>
      <Typography sx={{ fontSize: 12.5, color: 'text.secondary' }}>{description}</Typography>
    </ListItemButton>
  );

  return (
    <Dialog open={open} onClose={onClose} fullWidth maxWidth="sm">
      <DialogTitle variant="h3">New triggers</DialogTitle>
      <DialogContent>
        <TextField
          select
          fullWidth
          label="Network"
          value={chosen}
          onChange={(e) => setNetwork(e.target.value)}
          sx={{ mt: 1, mb: 2 }}
        >
          {networks.map((n) => (
            <MenuItem key={n} value={n}>
              {n}
            </MenuItem>
          ))}
        </TextField>
        <Stack spacing={0.5}>
          {option('Blank trigger', 'Start from nothing; add more triggers in the editor.', () => [
            blank(chain, name),
          ])}
          {TEMPLATES.map((t) =>
            option(
              t.name,
              `${t.description} Opens in the editor with every part filled in.`,
              () => t.make({ chain, network: name, token }),
              t.token && token === '',
            ),
          )}
        </Stack>
      </DialogContent>
    </Dialog>
  );
}
