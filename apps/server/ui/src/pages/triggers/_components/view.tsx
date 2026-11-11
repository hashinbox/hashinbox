import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Chip from '@mui/material/Chip';
import Stack from '@mui/material/Stack';
import Table from '@mui/material/Table';
import TableBody from '@mui/material/TableBody';
import TableCell from '@mui/material/TableCell';
import TableHead from '@mui/material/TableHead';
import TableRow from '@mui/material/TableRow';
import Typography from '@mui/material/Typography';
import { useEffect, useState, type ReactNode } from 'react';
import { call, client, type Trigger, type TriggerRun } from '@/api/client';
import { EmptyRow } from '@/_components/table';
import { When } from '@/_components/when';
import type { Shell } from '@/layout';
import { assetName } from '@/money';
import { mono } from '@/theme';
import {
  SELF,
  describeCompare,
  describeGate,
  describeLogic,
  describeSelector,
  describeSend,
  type Definition,
} from './forms';

/** How often the trigger, its last round and its runs are read again. */
const BEAT_MS = 5000;

/** What a round saw, in words: wallets selected, kept by the filter, the check, sends. */
function sawOf(stats: TriggerRun['stats']): string {
  const check = stats.checked ? 'check held' : 'check did not hold';
  return `${String(stats.selected)} selected · ${String(stats.kept)} kept · ${check} · ${String(stats.jobs)} sent`;
}

/** A stored trigger, read only: what it does, its last round and its runs. */
export function View({
  shell,
  initial,
  onEdit,
}: {
  shell: Shell;
  initial: Trigger;
  onEdit: () => void;
}) {
  const [trigger, setTrigger] = useState<Trigger>(initial);
  const [runs, setRuns] = useState<TriggerRun[]>([]);
  const id = initial.id;
  useEffect(() => {
    const read = () => {
      const params = { params: { path: { id } } };
      call(client.GET('/v1/triggers/{id}', params)).then(setTrigger).catch(shell.fail);
      call(client.GET('/v1/triggers/{id}/runs', params))
        .then((list) => setRuns(list.runs))
        .catch(shell.fail);
    };
    read();
    const timer = window.setInterval(read, BEAT_MS);
    return () => window.clearInterval(timer);
  }, [id, shell.fail]);

  // The server keeps the parts as the editor writes them.
  const parts = trigger as unknown as Definition;
  const name = (asset: string) => assetName(shell.listings, trigger.chain, trigger.network, asset);
  const last = trigger.last_round;

  return (
    <Box component="article" sx={{ maxWidth: 900 }}>
      <Stack direction="row" spacing={1.5} sx={{ alignItems: 'center', mb: 0.5 }}>
        <Typography variant="h2" component="h1" sx={{ fontFamily: mono, flexGrow: 1 }}>
          {trigger.id}
        </Typography>
        <Chip
          size="small"
          label={trigger.enabled ? 'on' : 'off'}
          color={trigger.enabled ? 'success' : 'default'}
        />
        <Button variant="outlined" size="small" onClick={onEdit}>
          Edit
        </Button>
      </Stack>
      <Typography variant="overline" component="p" sx={{ mb: 2 }}>
        {trigger.chain}/{trigger.network}
      </Typography>

      <Box
        component="dl"
        sx={{
          display: 'grid',
          gridTemplateColumns: 'max-content minmax(0, 1fr)',
          columnGap: 2,
          rowGap: 0.75,
          m: 0,
          fontSize: 13,
        }}
      >
        <Detail term="Wallets">{describeSelector(parts.selector, name)}</Detail>
        <Detail term="Filter">
          {parts.filter
            ? describeLogic(parts.filter, (f) => describeCompare(f.compare, name, SELF.one))
            : 'every one'}
        </Detail>
        <Detail term="When">
          {describeLogic(parts.check, (g) => describeGate(g, name, SELF.kept))}
        </Detail>
        <Detail term="Then">{describeSend(parts.action.input, name)}</Detail>
      </Box>

      <Typography variant="h4" component="h2" sx={{ mt: 3, mb: 0.5 }}>
        Last round
      </Typography>
      {last ? (
        <Typography sx={{ fontSize: 13 }}>
          <When at={last.at} /> at block #{last.confirmed}: {sawOf(last)}
          {last.refused && (
            <Box component="span" sx={{ display: 'block', color: 'error.main' }}>
              Refused: {last.refused}
            </Box>
          )}
        </Typography>
      ) : (
        <Typography sx={{ fontSize: 13, color: 'text.secondary' }}>
          {trigger.enabled ? 'No round yet.' : 'Off: rounds pass it by.'}
        </Typography>
      )}

      <Typography variant="h4" component="h2" sx={{ mt: 3, mb: 1 }}>
        Runs
      </Typography>
      <Table size="small">
        <TableHead>
          <TableRow>
            <TableCell>Started</TableCell>
            <TableCell>Selected</TableCell>
            <TableCell>Kept</TableCell>
            <TableCell>Sent</TableCell>
            <TableCell>Outcome</TableCell>
          </TableRow>
        </TableHead>
        <TableBody>
          {runs.length === 0 && <EmptyRow columns={5} message="It has not run yet." />}
          {runs.map((r) => (
            <TableRow key={r.started_at}>
              <TableCell>
                <When at={r.started_at} />
              </TableCell>
              <TableCell>{r.stats.selected}</TableCell>
              <TableCell>{r.stats.kept}</TableCell>
              <TableCell>{r.stats.jobs}</TableCell>
              <TableCell>{r.outcome ?? '—'}</TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </Box>
  );
}

/** One line of the details: its term, then what it says. */
function Detail({ term, children }: { term: string; children: ReactNode }) {
  return (
    <>
      <Box component="dt" sx={{ color: 'text.secondary' }}>
        {term}
      </Box>
      <Box component="dd" sx={{ m: 0 }}>
        {children}
      </Box>
    </>
  );
}
