import Add from '@mui/icons-material/Add';
import Bolt from '@mui/icons-material/Bolt';
import Box from '@mui/material/Box';
import IconButton from '@mui/material/IconButton';
import Tooltip from '@mui/material/Tooltip';
import { useCallback, useEffect, useState } from 'react';
import { call, client, type Listing, type Trigger, type Wallet } from '@/api/client';
import { assetName } from '@/money';
import { Empty } from '@/_components/empty';
import type { Shell } from '@/layout';
import { Confirm } from '@/_components/confirm';
import { Editor } from './_components/editor';
import { View } from './_components/view';
import { Compose } from './_components/compose';
import { Start } from './_components/start';
import type { Definition } from './_components/forms';
import { Split } from '@/_components/split';
import { Side, SideItem } from '@/_components/side';

/** What the right side shows: a stored trigger read or edited, new ones being written, or nothing. */
type Open =
  | { view: Trigger }
  | { edit: Trigger }
  | { compose: { title: string; drafts: Definition[] } }
  | null;

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

/** The stored trigger an open view or editor shows. */
function kept(open: { view: Trigger } | { edit: Trigger }): Trigger {
  return 'view' in open ? open.view : open.edit;
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
  // The trigger being deleted.
  const [deleting, setDeleting] = useState<Trigger | null>(null);

  const remove = (trigger: Trigger) => {
    setDeleting(null);
    call(client.DELETE('/v1/triggers/{id}', { params: { path: { id: trigger.id } } }))
      .then(() => {
        shell.notify('Trigger deleted.');
        refresh();
        setOpen((now) => (now && !('compose' in now) && kept(now).id === trigger.id ? null : now));
      })
      .catch(shell.fail);
  };

  const refresh = useCallback(() => {
    call(client.GET('/v1/triggers'))
      .then((list) => setTriggers(list.triggers))
      .catch(shell.fail);
  }, [shell.fail]);

  useEffect(refresh, [refresh]);
  useEffect(() => {
    call(client.GET('/v1/wallets/tags'))
      .then((list) => setTags(list.tags))
      .catch(shell.fail);
  }, [shell.fail]);

  const selected = open && !('compose' in open) ? kept(open).id : null;

  return (
    <>
      <Split
        columns="minmax(240px, 300px) minmax(0, 1fr)"
        open={open !== null}
        onClose={() => setOpen(null)}
        list={
          <Side
            title="Triggers"
            actions={
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
            }
            empty={triggers.length === 0 && 'No triggers yet. Start with +.'}
          >
            {triggers.map((t) => (
              <SideItem
                key={t.id}
                name={t.id}
                dot={{
                  color: t.enabled ? 'success.main' : 'error.main',
                  label: t.enabled ? 'On' : 'Off',
                }}
                selected={t.id === selected}
                onClick={() => setOpen({ view: t })}
                detail={`${t.chain}/${t.network} · ${summary(t, shell.listings)}`}
              />
            ))}
          </Side>
        }
        detail={
          <Box sx={{ px: { xs: 2, md: 4 }, py: { xs: 2.5, md: 3.5 }, minWidth: 0 }}>
            {open ? (
              'view' in open ? (
                <View
                  key={open.view.id}
                  shell={shell}
                  initial={open.view}
                  onEdit={() => setOpen({ edit: open.view })}
                  onDelete={() => setDeleting(open.view)}
                  onChanged={refresh}
                />
              ) : 'edit' in open ? (
                <Editor
                  key={open.edit.id}
                  shell={shell}
                  wallets={wallets}
                  tags={tags}
                  initial={definitionOf(open.edit)}
                  onSaved={(saved) => {
                    refresh();
                    setOpen({ view: saved });
                  }}
                  onCancel={() => setOpen({ view: open.edit })}
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
                    setOpen(first ? { view: first } : null);
                  }}
                />
              )
            ) : (
              <Empty icon={<Bolt />} title="Triggers">
                A trigger watches wallets and sends money on its own when a rule is met, like a
                balance going over a limit. Pick one to see what it does and how its rounds went;
                edit or delete it there. Add new ones with +.
              </Empty>
            )}
          </Box>
        }
      />
      <Confirm
        open={deleting !== null}
        title={`Delete ${deleting?.id ?? ''}`}
        phrase={deleting?.id ?? ''}
        action="Delete"
        onClose={() => setDeleting(null)}
        onConfirm={() => deleting && remove(deleting)}
      >
        Deletes the trigger and its run history. Sends already queued still go out.
      </Confirm>
      <Start
        shell={shell}
        open={choosing}
        onClose={() => setChoosing(false)}
        onStart={(title, drafts) => {
          setChoosing(false);
          setOpen({ compose: { title, drafts } });
        }}
      />
    </>
  );
}
