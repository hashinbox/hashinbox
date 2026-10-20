import Add from '@mui/icons-material/Add';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import IconButton from '@mui/material/IconButton';
import MenuItem from '@mui/material/MenuItem';
import Stack from '@mui/material/Stack';
import Tab from '@mui/material/Tab';
import Tabs from '@mui/material/Tabs';
import TextField from '@mui/material/TextField';
import Tooltip from '@mui/material/Tooltip';
import Typography from '@mui/material/Typography';
import { useState } from 'react';
import { call, client, type Trigger, type Wallet } from '@/api/client';
import type { Shell } from '@/layout';
import { mono } from '@/theme';
import { TriggerForm } from './form';
import { blank, type Definition } from './forms';

/**
 * New triggers, one tab each, created together: all or none. A template
 * opens this with its triggers filled in; a blank one starts with one.
 */
export function Compose({
  shell,
  wallets,
  tags,
  title,
  initial,
  onCreated,
  onCancel,
}: {
  shell: Shell;
  wallets: Wallet[];
  tags: string[];
  /** What these triggers are for: the template's name, or "New trigger". */
  title: string;
  initial: Definition[];
  onCreated: (triggers: Trigger[]) => void;
  onCancel: () => void;
}) {
  const [drafts, setDrafts] = useState<Definition[]>(initial);
  const [tab, setTab] = useState(0);
  const [busy, setBusy] = useState(false);
  const current = drafts[tab] ?? drafts[0];

  const set = (index: number, next: Definition) =>
    setDrafts((all) => all.map((d, i) => (i === index ? next : d)));
  // One batch, one network: choosing it moves every draft.
  const networks = [...new Set(shell.listings.map((l) => `${l.network.chain}/${l.network.name}`))];
  const network = current ? `${current.chain}/${current.network}` : '';
  const move = (to: string) => {
    const [chain = '', name = ''] = to.split('/');
    setDrafts((all) => all.map((d) => ({ ...d, chain, network: name })));
  };
  const add = () => {
    const place = current ?? initial[0];
    if (!place) return;
    setDrafts((all) => [...all, blank(place.chain, place.network)]);
    setTab(drafts.length);
  };
  const drop = (index: number) => {
    setDrafts((all) => all.filter((_, i) => i !== index));
    setTab((t) => Math.max(0, t >= index ? t - 1 : t));
  };

  const create = () => {
    setBusy(true);
    call(client.POST('/v1/triggers', { body: { triggers: drafts as never[] } }))
      .then((list) => onCreated(list.triggers))
      .catch(shell.fail)
      .finally(() => setBusy(false));
  };

  return (
    <Box component="article" sx={{ maxWidth: 900 }}>
      <Stack direction="row" spacing={2} sx={{ alignItems: 'center', mb: 1 }}>
        <Typography variant="h2" component="h1" sx={{ flexGrow: 1 }}>
          {title}
        </Typography>
        <Button color="inherit" onClick={onCancel}>
          Cancel
        </Button>
        <Button variant="contained" loading={busy} disabled={drafts.length === 0} onClick={create}>
          Create {drafts.length === 1 ? 'trigger' : `${drafts.length} triggers`}
        </Button>
      </Stack>
      <Stack direction="row" spacing={2} sx={{ alignItems: 'center', mb: 2 }}>
        <TextField
          select
          size="small"
          label="Network"
          value={network}
          onChange={(e) => move(e.target.value)}
          sx={{ minWidth: 220 }}
        >
          {networks.map((n) => (
            <MenuItem key={n} value={n}>
              {n}
            </MenuItem>
          ))}
        </TextField>
        <Typography variant="body2" color="text.secondary">
          Every trigger here is on this network and is created together with the others, or none is.
        </Typography>
      </Stack>
      <Stack
        direction="row"
        sx={{ alignItems: 'center', borderBottom: 1, borderColor: 'divider', mb: 3 }}
      >
        <Tabs value={Math.min(tab, drafts.length - 1)} onChange={(_, next: number) => setTab(next)}>
          {drafts.map((d, i) => (
            <Tab
              key={i}
              value={i}
              label={
                <Box component="span" sx={{ fontFamily: mono, fontSize: 13 }}>
                  {d.id || `trigger ${i + 1}`}
                </Box>
              }
            />
          ))}
        </Tabs>
        <Tooltip title="Add a trigger">
          <IconButton size="small" aria-label="Add a trigger" onClick={add} sx={{ ml: 1 }}>
            <Add fontSize="small" />
          </IconButton>
        </Tooltip>
        <Box sx={{ flexGrow: 1 }} />
        {drafts.length > 1 && (
          <Button size="small" color="error" onClick={() => drop(tab)}>
            Remove this one
          </Button>
        )}
      </Stack>
      {current && (
        <TriggerForm
          key={tab}
          shell={shell}
          wallets={wallets}
          tags={tags}
          value={current}
          onChange={(next) => set(tab, next)}
          kept={false}
        />
      )}
    </Box>
  );
}
