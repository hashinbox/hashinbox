import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Stack from '@mui/material/Stack';
import Table from '@mui/material/Table';
import TableBody from '@mui/material/TableBody';
import TableCell from '@mui/material/TableCell';
import TableHead from '@mui/material/TableHead';
import TableRow from '@mui/material/TableRow';
import Typography from '@mui/material/Typography';
import { useEffect, useState } from 'react';
import { call, client, type Trigger, type TriggerRun, type Wallet } from '@/api/client';
import { Confirm } from '@/_components/confirm';
import { EmptyRow } from '@/_components/table';
import type { Shell } from '@/layout';
import { mono } from '@/theme';
import { TriggerForm } from './form';
import type { Definition } from './forms';

/** A stored trigger: its form, saving, deleting, and its runs. */
export function Editor({
  shell,
  wallets,
  tags,
  initial,
  onSaved,
  onDeleted,
}: {
  shell: Shell;
  wallets: Wallet[];
  tags: string[];
  initial: Definition;
  onSaved: (trigger: Trigger) => void;
  onDeleted: () => void;
}) {
  const [def, setDef] = useState<Definition>(initial);
  const [busy, setBusy] = useState(false);
  const [deleting, setDeleting] = useState(false);

  const save = () => {
    setBusy(true);
    const { id, ...body } = def;
    call(client.PUT('/v1/triggers/{id}', { params: { path: { id } }, body: body as never }))
      .then((saved) => {
        shell.notify('Trigger saved.');
        onSaved(saved);
      })
      .catch(shell.fail)
      .finally(() => setBusy(false));
  };

  const remove = () => {
    setDeleting(false);
    call(client.DELETE('/v1/triggers/{id}', { params: { path: { id: def.id } } }))
      .then(() => {
        shell.notify('Trigger deleted.');
        onDeleted();
      })
      .catch(shell.fail);
  };

  return (
    <Box component="article" sx={{ maxWidth: 900 }}>
      <Stack direction="row" spacing={2} sx={{ alignItems: 'center', mb: 2 }}>
        <Typography variant="h2" component="h1" sx={{ fontFamily: mono, flexGrow: 1 }}>
          {def.id}
        </Typography>
        <Button variant="contained" loading={busy} onClick={save}>
          Save
        </Button>
        <Button color="error" onClick={() => setDeleting(true)}>
          Delete
        </Button>
      </Stack>
      <TriggerForm shell={shell} wallets={wallets} tags={tags} value={def} onChange={setDef} kept />
      <Runs shell={shell} id={def.id} />
      <Confirm
        open={deleting}
        title={`Delete ${def.id}`}
        phrase={def.id}
        action="Delete"
        onClose={() => setDeleting(false)}
        onConfirm={remove}
      >
        Deletes the trigger and its run history. Sends already queued still go out.
      </Confirm>
    </Box>
  );
}

function Runs({ shell, id }: { shell: Shell; id: string }) {
  const [runs, setRuns] = useState<TriggerRun[]>([]);
  useEffect(() => {
    call(client.GET('/v1/triggers/{id}/runs', { params: { path: { id } } }))
      .then((list) => setRuns(list.runs))
      .catch(shell.fail);
  }, [id, shell.fail]);
  const time = (ms: number) => new Date(ms).toLocaleString();
  return (
    <Box sx={{ mt: 3 }}>
      <Typography variant="h4" component="h2" sx={{ mb: 1 }}>
        Runs
      </Typography>
      <Table size="small">
        <TableHead>
          <TableRow>
            <TableCell>Started</TableCell>
          </TableRow>
        </TableHead>
        <TableBody>
          {runs.length === 0 && <EmptyRow columns={1} message="It has not run yet." />}
          {runs.map((r) => (
            <TableRow key={r.started_at}>
              <TableCell>{time(r.started_at)}</TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </Box>
  );
}
