import Box from '@mui/material/Box';
import MenuItem from '@mui/material/MenuItem';
import Stack from '@mui/material/Stack';
import Table from '@mui/material/Table';
import TableBody from '@mui/material/TableBody';
import TableCell from '@mui/material/TableCell';
import TableHead from '@mui/material/TableHead';
import TableRow from '@mui/material/TableRow';
import Tab from '@mui/material/Tab';
import Tabs from '@mui/material/Tabs';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useCallback, useEffect, useState } from 'react';
import { call, client, type Job } from '@/api/client';
import type { Shell } from '@/layout';
import { EmptyRow } from '@/_components/table';
import { mono } from '@/theme';

const BEAT_MS = 4000;
/** Every job kind there is, as the tabs show them. */
const KINDS = [
  { kind: 'triggers', label: 'Triggers' },
  { kind: 'broadcast', label: 'Broadcast' },
  { kind: 'index', label: 'Index' },
  { kind: 'expire', label: 'Expire' },
] as const;

function labelOf(kind: string): string {
  return KINDS.find((k) => k.kind === kind)?.label ?? kind;
}
const STATES = ['pending', 'running', 'succeeded', 'failed'] as const;
const TONE: Record<Job['state'], string> = {
  pending: 'text.disabled',
  running: 'info.main',
  succeeded: 'success.main',
  failed: 'error.main',
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

/** `ms` as a local time; within the next minute, as seconds from now. */
function when(ms: number, now: number): string {
  const ahead = ms - now;
  if (ahead > 0 && ahead < 60_000) return `in ${Math.ceil(ahead / 1000)} s`;
  return new Date(ms).toLocaleString();
}

/** The queue, one tab per kind, each latest written first. */
export function Jobs({ shell }: { shell: Shell }) {
  const [jobs, setJobs] = useState<Job[]>([]);
  const [kind, setKind] = useState<string>(KINDS[0].kind);
  const [state, setState] = useState<string>('');
  const [selected, setSelected] = useState<string | null>(null);
  const [now, setNow] = useState(() => Date.now());

  const refresh = useCallback(() => {
    const query = { kind, ...(state ? { state: state as Job['state'] } : {}) };
    call(client.GET('/v1/jobs', { params: { query } }))
      .then((list) => {
        setJobs(list.jobs);
        setNow(Date.now());
      })
      .catch(shell.fail);
  }, [kind, state, shell.fail]);

  useEffect(() => {
    refresh();
    const timer = window.setInterval(refresh, BEAT_MS);
    return () => window.clearInterval(timer);
  }, [refresh]);

  const current = jobs.find((j) => j.id === selected) ?? null;

  return (
    <Box
      sx={{
        display: 'grid',
        gridTemplateColumns: { xs: 'minmax(0, 1fr)', lg: 'minmax(0, 3fr) minmax(320px, 2fr)' },
        minHeight: 'calc(100vh - 65px)',
      }}
    >
      <Box sx={{ px: { xs: 2, md: 4 }, py: { xs: 2.5, md: 3.5 }, minWidth: 0 }}>
        <Stack direction="row" spacing={2} sx={{ alignItems: 'center', mb: 2, flexWrap: 'wrap' }}>
          <Typography variant="h3" sx={{ flexGrow: 1 }}>
            Jobs
          </Typography>
          <TextField
            select
            size="small"
            label="State"
            value={state}
            onChange={(e) => setState(e.target.value)}
            sx={{ minWidth: 140 }}
          >
            <MenuItem value="">every state</MenuItem>
            {STATES.map((s) => (
              <MenuItem key={s} value={s}>
                {s}
              </MenuItem>
            ))}
          </TextField>
        </Stack>
        <Tabs
          value={kind}
          onChange={(_, next: string) => {
            setKind(next);
            setSelected(null);
          }}
          aria-label="Job kinds"
          sx={{ mb: 1 }}
        >
          {KINDS.map((k) => (
            <Tab key={k.kind} value={k.kind} label={k.label} />
          ))}
        </Tabs>
        <Table size="small">
          <TableHead>
            <TableRow>
              <TableCell sx={{ width: 24 }} />
              <TableCell>Topic</TableCell>
              <TableCell>Run at</TableCell>
              <TableCell align="right">Attempts</TableCell>
              <TableCell>Reason</TableCell>
            </TableRow>
          </TableHead>
          <TableBody>
            {jobs.length === 0 && <EmptyRow columns={5} message="No job matches." />}
            {jobs.map((j) => (
              <TableRow
                key={j.id}
                hover
                selected={j.id === selected}
                onClick={() => setSelected(j.id)}
                sx={{ cursor: 'pointer' }}
              >
                <TableCell>
                  <Dot state={j.state} />
                </TableCell>
                <TableCell sx={figures}>{j.topic}</TableCell>
                <TableCell sx={figures}>{when(j.run_at, now)}</TableCell>
                <TableCell align="right" sx={figures}>
                  {j.attempts}
                </TableCell>
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
      </Box>

      <Box
        component="aside"
        sx={(t) => ({
          px: { xs: 2, md: 3 },
          py: { xs: 2.5, md: 3.5 },
          minWidth: 0,
          borderTop: `1px solid ${t.palette.divider}`,
          [t.breakpoints.up('lg')]: { borderTop: 0, borderLeft: `1px solid ${t.palette.divider}` },
        })}
      >
        {current ? (
          <Detail job={current} now={now} />
        ) : (
          <Typography color="text.secondary">
            Pick a job to see what it was given and did.
          </Typography>
        )}
      </Box>
    </Box>
  );
}

/** One job whole: where it is, what it was given, what its worker did. */
function Detail({ job, now }: { job: Job; now: number }) {
  const facts: [string, string][] = [
    ['id', job.id],
    ['kind', job.kind],
    ['topic', job.topic],
    ['state', job.state],
    ['run at', when(job.run_at, now)],
    ['deadline', job.deadline === null ? '—' : new Date(job.deadline).toLocaleString()],
    ['attempts', String(job.attempts)],
    ...(job.kind === 'broadcast'
      ? ([
          ['signed', String(job.signed)],
          ['accepted', String(job.accepted)],
        ] as [string, string][])
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
