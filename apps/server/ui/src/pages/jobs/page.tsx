import FilterList from '@mui/icons-material/FilterList';
import WorkHistory from '@mui/icons-material/WorkHistory';
import Autocomplete from '@mui/material/Autocomplete';
import Box from '@mui/material/Box';
import Chip from '@mui/material/Chip';
import Stack from '@mui/material/Stack';
import Table from '@mui/material/Table';
import TableBody from '@mui/material/TableBody';
import TableCell from '@mui/material/TableCell';
import TableContainer from '@mui/material/TableContainer';
import TableHead from '@mui/material/TableHead';
import TableRow from '@mui/material/TableRow';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useCallback, useEffect, useState, type ReactNode } from 'react';
import { call, client, type Job } from '@/api/client';
import type { Shell } from '@/layout';
import { Empty } from '@/_components/empty';
import { EmptyRow } from '@/_components/table';
import { shorten } from '@/_components/trim';
import { When } from '@/_components/when';
import { mono } from '@/theme';
import { Split } from '@/_components/split';

const BEAT_MS = 4000;
/** Every job kind there is, as the filter names them. */
const KINDS = [
  { kind: 'triggers', label: 'Triggers' },
  { kind: 'broadcast', label: 'Broadcast' },
  { kind: 'index', label: 'Index' },
  { kind: 'prune', label: 'Prune' },
] as const;

function labelOf(kind: string): string {
  return KINDS.find((k) => k.kind === kind)?.label ?? kind;
}
const STATES = ['pending', 'running', 'succeeded', 'failed', 'skipped', 'idle'] as const;

/** One tag of the filter: a network, a kind or a state. */
interface Filter {
  group: 'network' | 'kind' | 'state';
  value: string;
  label: string;
}
const TONE: Record<Job['state'], string> = {
  pending: 'text.disabled',
  running: 'info.main',
  succeeded: 'success.main',
  failed: 'error.main',
  skipped: 'text.disabled',
  idle: 'text.disabled',
};

const figures = { fontFamily: mono, fontSize: 12, fontVariantNumeric: 'tabular-nums' } as const;

function Dot({ state }: { state: Job['state'] }) {
  return (
    <Box
      component="span"
      aria-label={state}
      sx={{
        display: 'inline-block',
        width: 8,
        height: 8,
        borderRadius: '50%',
        bgcolor: TONE[state],
      }}
    />
  );
}

/** The network a job's input names; none for `prune`, whose input is null. */
function networkOf(job: Job): string {
  const input = job.input as { chain?: unknown; network?: unknown } | null;
  return typeof input?.chain === 'string' && typeof input.network === 'string'
    ? `${input.chain}/${input.network}`
    : '—';
}

/** When a pending job runs next, counting down; nothing for one that already ran. */
function next(job: Job): ReactNode {
  if (job.state !== 'pending') return '—';
  // 0: as soon as a worker is free.
  return job.run_at === 0 ? 'now' : <When at={job.run_at} due />;
}

/** The queue, latest written first, filtered by network, kind and state. */
export function Jobs({ shell }: { shell: Shell }) {
  const [jobs, setJobs] = useState<Job[]>([]);
  const [tags, setTags] = useState<Filter[]>([]);
  const valueOf = (group: Filter['group']) => tags.find((t) => t.group === group)?.value ?? '';
  const network = valueOf('network');
  const kind = valueOf('kind');
  const state = valueOf('state');
  const [selected, setSelected] = useState<string | null>(null);

  const refresh = useCallback(() => {
    const [chainName, networkName] = network.split('/');
    const query = {
      ...(chainName && networkName ? { chain: chainName, network: networkName } : {}),
      ...(kind ? { kind } : {}),
      ...(state ? { state: state as Job['state'] } : {}),
    };
    call(client.GET('/v1/jobs', { params: { query } }))
      .then((list) => {
        setJobs(list.jobs);
      })
      .catch(shell.fail);
  }, [network, kind, state, shell.fail]);

  useEffect(() => {
    refresh();
    const timer = window.setInterval(refresh, BEAT_MS);
    return () => window.clearInterval(timer);
  }, [refresh]);

  const current = jobs.find((j) => j.id === selected) ?? null;
  const networks = [
    ...new Set(shell.listings.map((l) => `${l.network.chain}/${l.network.name}`)),
  ].sort();
  const options: Filter[] = [
    ...networks.map((n) => ({ group: 'network' as const, value: n, label: n })),
    ...KINDS.map((k) => ({ group: 'kind' as const, value: k.kind, label: k.label })),
    ...STATES.map((v) => ({ group: 'state' as const, value: v, label: v })),
  ];
  return (
    <Split
      columns="minmax(0, 3fr) minmax(320px, 2fr)"
      wide="lg"
      open={selected !== null}
      onClose={() => setSelected(null)}
      list={
        <Box sx={{ px: { xs: 2, md: 4 }, py: { xs: 2.5, md: 3.5 }, minWidth: 0 }}>
          <Autocomplete
            multiple
            size="small"
            options={options}
            value={tags}
            groupBy={(o) => o.group}
            getOptionLabel={(o) => o.label}
            isOptionEqualToValue={(o, v) => o.group === v.group && o.value === v.value}
            onChange={(_, next) => {
              // One tag per group: a newer one replaces the older.
              setTags(next.filter((t, i) => !next.slice(i + 1).some((u) => u.group === t.group)));
              setSelected(null);
            }}
            renderValue={(value, getItemProps) =>
              value.map((t, index) => (
                <Chip
                  {...getItemProps({ index })}
                  key={`${t.group}:${t.value}`}
                  size="small"
                  label={
                    <>
                      <Box component="span" sx={{ color: 'text.secondary', mr: 0.5 }}>
                        {t.group}
                      </Box>
                      {t.label}
                    </>
                  }
                  sx={{ borderRadius: '6px', bgcolor: 'grey.100', fontSize: 12 }}
                />
              ))
            }
            renderInput={(params) => (
              <TextField
                {...params}
                placeholder={tags.length === 0 ? 'Filter by network, kind or state' : ''}
                slotProps={{
                  ...params.slotProps,
                  input: {
                    ...params.slotProps.input,
                    startAdornment: (
                      <>
                        <FilterList
                          fontSize="small"
                          aria-hidden
                          sx={{ color: 'text.secondary', ml: 0.5, mr: 0.25 }}
                        />
                        {params.slotProps.input.startAdornment}
                      </>
                    ),
                  },
                  htmlInput: { ...params.slotProps.htmlInput, 'aria-label': 'Filter jobs' },
                }}
                sx={{ '& .MuiOutlinedInput-root': { gap: 0.5, fontSize: 13 } }}
              />
            )}
            sx={{ mb: 2, maxWidth: 720 }}
          />
          {/* Wider than its column: it scrolls inside, never over the detail beside it. */}
          <TableContainer>
            <Table size="small">
              <TableHead>
                <TableRow>
                  <TableCell>Job</TableCell>
                  <TableCell>Kind</TableCell>
                  <TableCell>Network</TableCell>
                  <TableCell>Created</TableCell>
                  <TableCell>Updated</TableCell>
                  <TableCell>Next run</TableCell>
                  <TableCell>Attempts</TableCell>
                  <TableCell>Reason</TableCell>
                </TableRow>
              </TableHead>
              <TableBody>
                {jobs.length === 0 && <EmptyRow columns={8} message="No job matches." />}
                {jobs.map((j) => (
                  <TableRow
                    key={j.id}
                    hover
                    selected={j.id === selected}
                    onClick={() => setSelected(j.id)}
                    sx={{ cursor: 'pointer' }}
                  >
                    <TableCell sx={{ ...figures, whiteSpace: 'nowrap' }} title={j.id}>
                      <Dot state={j.state} /> {shorten(j.id)}
                    </TableCell>
                    <TableCell>{labelOf(j.kind)}</TableCell>
                    <TableCell sx={figures}>{networkOf(j)}</TableCell>
                    <TableCell sx={figures}>
                      <When at={j.created_at} />
                    </TableCell>
                    <TableCell sx={figures}>
                      <When at={j.updated_at} />
                    </TableCell>
                    <TableCell sx={figures}>{next(j)}</TableCell>
                    <TableCell sx={figures}>{j.attempts}</TableCell>
                    <TableCell
                      sx={{
                        ...figures,
                        color: j.state === 'failed' ? 'error.main' : 'text.secondary',
                        maxWidth: 280,
                        overflow: 'hidden',
                        textOverflow: 'ellipsis',
                        whiteSpace: 'nowrap',
                      }}
                    >
                      {j.reason ?? ''}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </TableContainer>
        </Box>
      }
      detail={
        <Box
          component="aside"
          sx={(t) => ({
            px: { xs: 2, md: 3 },
            py: { xs: 2.5, md: 3.5 },
            minWidth: 0,
            borderTop: `1px solid ${t.palette.divider}`,
            [t.breakpoints.up('lg')]: {
              borderTop: 0,
              borderLeft: `1px solid ${t.palette.divider}`,
            },
          })}
        >
          {/* Held in view while the list beside it scrolls. */}
          <Box
            sx={{
              position: { lg: 'sticky' },
              top: { lg: 88 },
              height: { lg: 'calc(100vh - 120px)' },
              overflowY: { lg: 'auto' },
            }}
          >
            {current ? (
              <Detail job={current} />
            ) : (
              <Empty icon={<WorkHistory />} title="Jobs">
                A job is a piece of background work, like indexing a network, checking triggers or
                sending a payment, retried until it is done. Pick one to see its input, progress and
                why it last stopped.
              </Empty>
            )}
          </Box>
        </Box>
      }
    />
  );
}

/** One job whole: where it is, what it was given, what its worker did. */
function Detail({ job }: { job: Job }) {
  const facts: [string, ReactNode][] = [
    ['id', job.id],
    ['kind', job.kind],
    ['topic', job.topic],
    ['state', job.state],
    ['next run', next(job)],
    ['deadline', job.deadline === null ? '—' : <When at={job.deadline} />],
    ['attempts', String(job.attempts)],
    ...(job.kind === 'broadcast'
      ? ([
          ['txid', job.txid ?? '—'],
          ['accepted', String(job.accepted)],
        ] as [string, ReactNode][])
      : []),
  ];
  return (
    <Stack spacing={2}>
      <Stack direction="row" spacing={1} sx={{ alignItems: 'center' }}>
        <Dot state={job.state} />
        <Typography variant="h4" component="h2">
          {labelOf(job.kind)}
        </Typography>
      </Stack>
      <Box
        component="dl"
        sx={{ m: 0, display: 'grid', gridTemplateColumns: 'auto 1fr', gap: 0.75 }}
      >
        {facts.map(([label, value]) => (
          <Box key={label} sx={{ display: 'contents' }}>
            <Typography component="dt" sx={{ fontSize: 12, color: 'text.secondary', pr: 2 }}>
              {label}
            </Typography>
            <Typography component="dd" sx={{ ...figures, m: 0, wordBreak: 'break-all' }}>
              {value}
            </Typography>
          </Box>
        ))}
      </Box>
      {job.reason && (
        <Section title="Reason">
          <Typography sx={{ ...figures, color: job.state === 'failed' ? 'error.main' : undefined }}>
            {job.reason}
          </Typography>
        </Section>
      )}
      <Section title="Input">
        <Json value={job.input} />
      </Section>
      <Section title="Progress">
        {job.progress == null ? (
          <Typography sx={{ fontSize: 12, color: 'text.secondary' }}>None saved.</Typography>
        ) : (
          <Json value={job.progress} />
        )}
      </Section>
    </Stack>
  );
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <Box>
      <Typography variant="overline">{title}</Typography>
      {children}
    </Box>
  );
}

function Json({ value }: { value: unknown }) {
  return (
    <Box
      component="pre"
      sx={(t) => ({
        ...figures,
        m: 0,
        p: 1.5,
        border: `1px solid ${t.palette.divider}`,
        overflowX: 'auto',
      })}
    >
      {JSON.stringify(value, null, 2)}
    </Box>
  );
}
