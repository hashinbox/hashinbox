import Add from '@mui/icons-material/Add';
import Bolt from '@mui/icons-material/Bolt';
import Box from '@mui/material/Box';
import Chip from '@mui/material/Chip';
import IconButton from '@mui/material/IconButton';
import List from '@mui/material/List';
import ListItemButton from '@mui/material/ListItemButton';
import Stack from '@mui/material/Stack';
import Tooltip from '@mui/material/Tooltip';
import Typography from '@mui/material/Typography';
import { useCallback, useEffect, useState } from 'react';
import { call, client, type Listing, type Trigger, type Wallet } from '@/api/client';
import { assetName } from '@/money';
import { Empty } from '@/_components/empty';
import type { Shell } from '@/layout';
import { mono } from '@/theme';
import { Editor } from './_components/editor';
import { Compose } from './_components/compose';
import { Start } from './_components/start';
import type { Definition } from './_components/forms';

/** What the right side shows: a stored trigger, new ones being written, or nothing. */
type Open = { kept: Trigger } | { compose: { title: string; drafts: Definition[] } } | null;

/** A kept trigger as the editor holds it. */
function definitionOf(trigger: Trigger): Definition {
  return {
    id: trigger.id,
    chain: trigger.chain,
    network: trigger.network,
    selector: trigger.selector as unknown as Definition['selector'],
    filter: (trigger.filter ?? null) as unknown as Definition['filter'],
    check: trigger.check as unknown as Definition['check'],
    action: trigger.action as unknown as Definition['action'],
    enabled: trigger.enabled,
  };
}

/** One line on what a trigger does. */
function summary(trigger: Trigger, listings: Listing[]): string {
  const send = definitionOf(trigger).action.input;
  return `send ${assetName(listings, trigger.chain, trigger.network, send.asset)}`;
}

/** The triggers beside the one being edited. */
export function Triggers({ shell, wallets }: { shell: Shell; wallets: Wallet[] }) {
  const [triggers, setTriggers] = useState<Trigger[]>([]);
  const [open, setOpen] = useState<Open>(null);
  const [choosing, setChoosing] = useState(false);
  const [tags, setTags] = useState<string[]>([]);

  const refresh = useCallback(() => {
    call(client.GET('/v1/triggers'))
      .then((list) => setTriggers(list.triggers))
      .catch(shell.fail);
  }, [shell.fail]);

  useEffect(refresh, [refresh]);
  useEffect(() => {
    call(client.GET('/v1/wallets/tags'))
      .then((list) => setTags([...new Set(list.tags.map((t) => t.tag))].sort()))
      .catch(shell.fail);
  }, [shell.fail]);

  const selected = open && 'kept' in open ? open.kept.id : null;

  return (
    <Box
      sx={{
        display: 'grid',
        gridTemplateColumns: { xs: 'minmax(0, 1fr)', md: 'minmax(240px, 300px) minmax(0, 1fr)' },
        minHeight: 'calc(100vh - 65px)',
      }}
    >
      <Box
        component="aside"
        sx={(t) => ({
          p: 2.5,
          minWidth: 0,
          borderBottom: `1px solid ${t.palette.divider}`,
          [t.breakpoints.up('md')]: {
            borderBottom: 0,
            borderRight: `1px solid ${t.palette.divider}`,
          },
        })}
      >
        <Stack direction="row" sx={{ alignItems: 'center', mb: 1.5 }}>
          <Typography variant="h3" sx={{ flexGrow: 1 }}>
            Triggers
          </Typography>
          <Tooltip title="New trigger">
            <IconButton
              size="small"
              aria-label="New trigger"
              onClick={(e) => {
                e.currentTarget.blur();
                setChoosing(true);
              }}
            >
              <Add fontSize="small" />
            </IconButton>
          </Tooltip>
        </Stack>
        <List disablePadding aria-label="Triggers">
          {triggers.map((t) => (
            <ListItemButton
              key={t.id}
              selected={t.id === selected}
              onClick={() => setOpen({ kept: t })}
              sx={{ py: 1.25, px: 1, display: 'block' }}
            >
              <Stack direction="row" spacing={1} sx={{ alignItems: 'center' }}>
                <Typography noWrap sx={{ fontFamily: mono, fontSize: 13, flexGrow: 1 }}>
                  {t.id}
                </Typography>
                <Chip
                  size="small"
                  label={t.enabled ? 'on' : 'off'}
                  color={t.enabled ? 'success' : 'default'}
                />
              </Stack>
              <Typography noWrap sx={{ fontSize: 11, color: 'text.secondary' }}>
                {t.chain}/{t.network} · {summary(t, shell.listings)}
              </Typography>
            </ListItemButton>
          ))}
          {triggers.length === 0 && (
            <Typography color="text.secondary" sx={{ py: 2, fontSize: 13 }}>
              No triggers yet. Start with +.
            </Typography>
          )}
        </List>
      </Box>

      <Box sx={{ px: { xs: 2, md: 4 }, py: { xs: 2.5, md: 3.5 }, minWidth: 0 }}>
        {open ? (
          'kept' in open ? (
            <Editor
              key={open.kept.id}
              shell={shell}
              wallets={wallets}
              tags={tags}
              initial={definitionOf(open.kept)}
              onSaved={(saved) => {
                refresh();
                setOpen({ kept: saved });
              }}
              onDeleted={() => {
                refresh();
                setOpen(null);
              }}
            />
          ) : (
            <Compose
              key={`${open.compose.title}:${String(open.compose.drafts.length)}`}
              shell={shell}
              wallets={wallets}
              tags={tags}
              title={open.compose.title}
              initial={open.compose.drafts}
              onCancel={() => setOpen(null)}
              onCreated={(created) => {
                refresh();
                shell.notify(`Created ${created.map((t) => t.id).join(', ')}.`);
                const first = created[0];
                setOpen(first ? { kept: first } : null);
              }}
            />
          )
        ) : (
          <Empty icon={<Bolt />} title="Triggers">
            A trigger watches wallets and sends money on its own when a rule is met, like a balance
            going over a limit. Pick one to edit its rule and action, or add new ones with +.
          </Empty>
        )}
      </Box>

      <Start
        shell={shell}
        open={choosing}
        onClose={() => setChoosing(false)}
        onStart={(title, drafts) => {
          setChoosing(false);
          setOpen({ compose: { title, drafts } });
        }}
      />
    </Box>
  );
}
