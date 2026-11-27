import Add from '@mui/icons-material/Add';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import IconButton from '@mui/material/IconButton';
import Stack from '@mui/material/Stack';
import Tab from '@mui/material/Tab';
import Tabs from '@mui/material/Tabs';
import Tooltip from '@mui/material/Tooltip';
import Typography from '@mui/material/Typography';
import { useState } from 'react';
import { triggersCreate, type Trigger, type Wallet } from '@hashinbox/client';
import { api } from '@/api/client';
import { mono } from '@/theme';
import { TriggerForm } from './form';
import { blank, validId, type Definition } from './forms';
import { fail } from '@/store/app';

/**
 * New triggers, one tab each, created together: all or none. A template
 * opens this with its triggers filled in; a blank one starts with one.
 */
export function Compose({
  wallets,
  tags,
  title,
  initial,
  onCreated,
  onCancel,
}: {
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

  // One batch, one network: choosing it on one draft moves every draft.
  const change = (index: number, next: Definition) =>
    setDrafts((all) =>
      all.map((d, i) => (i === index ? next : { ...d, chain: next.chain, network: next.network })),
    );
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
    triggersCreate(api, { body: { triggers: drafts } })
      .then((list) => onCreated(list.triggers))
      .catch(fail)
      .finally(() => setBusy(false));
  };

  return (
    <Box component="article" sx={{ maxWidth: 900 }}>
      <Stack direction="row" spacing={2} sx={{ alignItems: 'center', mb: 2 }}>
        <Typography variant="h2" component="h1" sx={{ flexGrow: 1 }}>
          {title}
        </Typography>
        <Button color="inherit" onClick={onCancel}>
          Cancel
        </Button>
        <Button
          variant="contained"
          loading={busy}
          disabled={drafts.length === 0 || drafts.some((d) => !validId(d.id))}
          onClick={create}
        >
          Create {drafts.length === 1 ? 'trigger' : `${drafts.length} triggers`}
        </Button>
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
          wallets={wallets}
          tags={tags}
          value={current}
          onChange={(next) => change(tab, next)}
          kept={false}
        />
      )}
    </Box>
  );
}
