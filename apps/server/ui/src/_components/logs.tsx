import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Link from '@mui/material/Link';
import Stack from '@mui/material/Stack';
import Typography from '@mui/material/Typography';
import { useCallback, useEffect, useRef, useState } from 'react';
import type { Shell } from '@/layout';
import { go, href } from '@/route';
import { mono } from '@/theme';

/** One kept entry as a page or the stream holds it; a live one's id is 0. */
interface Entry {
  id: number;
  /** Unix ms. */
  at: number;
  level: string;
  target: string;
  message: string;
  fields: Record<string, string>;
  chain: string | null;
  network: string | null;
  job_id: number | null;
}

/** One event of the stream: its name and its JSON data. */
interface Heard {
  event: string;
  data: unknown;
}

/** Hands each event of the stream to `onEvent` until `signal` aborts. */
type Listen = (onEvent: (heard: Heard) => void, signal: AbortSignal) => Promise<void>;

/** One page of entries, newest first; `next_cursor` reads the one after. */
interface Page {
  logs: Entry[];
  next_cursor: string | null;
}

/** An entry as shown, with a key of its own: a live one has no id. */
interface Row {
  key: string;
  entry: Entry;
}

/** The rows shown, and whether older ones are left to read. */
interface Shown {
  rows: Row[];
  more: boolean;
}

/** The most rows held; beyond them the oldest go. */
const MOST = 2000;

const TONE: Record<string, string> = {
  error: 'error.main',
  warn: 'warning.main',
  info: 'text.primary',
  debug: 'text.secondary',
  trace: 'text.disabled',
};

/** Live rows so far, for their keys. */
let lived = 0;

function stored(entry: Entry): Row {
  return { key: String(entry.id), entry };
}

/** `fresh` on top of `shown`, at most `MOST` kept. */
function prepend(shown: Shown, fresh: Row[]): Shown {
  const rows = [...fresh, ...shown.rows];
  return { rows: rows.slice(0, MOST), more: shown.more || rows.length > MOST };
}

/**
 * Kept log entries, newest first: when, level, target, message, network and
 * job, and every field as `key=val`, folded; "Older" reads the page after the
 * last one shown. `read` asks for one page, after `cursor` when given.
 * `listen`, when given, streams each new entry on top, live or paused.
 */
export function Logs({
  shell,
  read,
  listen,
}: {
  shell: Shell;
  read: (cursor: string | undefined) => Promise<Page>;
  listen?: Listen;
}) {
  const [shown, setShown] = useState<Shown>({ rows: [], more: false });
  const [waiting, setWaiting] = useState<Row[]>([]);
  const [paused, setPaused] = useState(false);
  const pause = useRef(false);
  const { fail, notify } = shell;

  useEffect(() => {
    const abort = new AbortController();
    read(undefined)
      .then((page) => {
        setShown({ rows: page.logs.map(stored), more: page.next_cursor !== null });
        if (listen === undefined) return;
        return listen(({ event, data }) => {
          if (event === 'missed') {
            notify(`${(data as { missed: number }).missed} log entries missed.`, true);
            return;
          }
          const row = { key: `live-${(lived += 1)}`, entry: data as Entry };
          if (pause.current) setWaiting((rows) => [row, ...rows].slice(0, MOST));
          else setShown((s) => prepend(s, [row]));
        }, abort.signal);
      })
      .catch((error: unknown) => {
        if (!abort.signal.aborted) fail(error);
      });
    return () => abort.abort();
  }, [read, listen, fail, notify]);

  const toggle = () => {
    pause.current = !paused;
    setPaused(!paused);
    if (!paused) return;
    setShown((s) => prepend(s, waiting));
    setWaiting([]);
  };

  const older = useCallback(() => {
    // A page's cursor is its last entry's id: the oldest stored row shown.
    const last = [...shown.rows].reverse().find((row) => row.entry.id > 0);
    read(last ? String(last.entry.id) : undefined)
      .then((page) => {
        setShown((s) => ({
          rows: [...s.rows, ...page.logs.map(stored)],
          more: page.next_cursor !== null,
        }));
      })
      .catch(fail);
  }, [shown.rows, read, fail]);

  return (
    <Stack spacing={0.5}>
      {listen !== undefined && (
        <Stack direction="row" spacing={1.5} sx={{ alignItems: 'center', pb: 1 }}>
          <Button size="small" onClick={toggle}>
            {paused ? 'Live' : 'Pause'}
          </Button>
          <Typography color="text.secondary" sx={{ fontSize: 13 }}>
            {paused ? `Paused, ${waiting.length} new` : 'Live'}
          </Typography>
        </Stack>
      )}
      {shown.rows.map((row) => (
        <Line key={row.key} entry={row.entry} />
      ))}
      {shown.rows.length === 0 && (
        <Typography color="text.secondary" sx={{ fontSize: 13 }}>
          No entry is kept.
        </Typography>
      )}
      {shown.more && (
        <Box>
          <Button size="small" onClick={older}>
            Older
          </Button>
        </Box>
      )}
    </Stack>
  );
}

/** `at` (Unix ms) as local `YYYY-MM-DD HH:MM:SS.mmm`. */
function stamp(at: number): string {
  const d = new Date(at);
  const two = (n: number) => String(n).padStart(2, '0');
  const date = `${d.getFullYear()}-${two(d.getMonth() + 1)}-${two(d.getDate())}`;
  const time = `${two(d.getHours())}:${two(d.getMinutes())}:${two(d.getSeconds())}`;
  return `${date} ${time}.${String(d.getMilliseconds()).padStart(3, '0')}`;
}

/** A link within the page to `parts`. */
function To({ parts, children }: { parts: string[]; children: string }) {
  return (
    <Link
      href={href(parts)}
      underline="hover"
      onClick={(event) => {
        event.preventDefault();
        go(parts);
      }}
      sx={{ mr: 1 }}
    >
      {children}
    </Link>
  );
}

/** One entry on one line; its fields fold out below. */
function Line({ entry }: { entry: Entry }) {
  const [open, setOpen] = useState(false);
  const fields = Object.entries(entry.fields);
  return (
    <Box sx={{ fontFamily: mono, fontSize: 12, wordBreak: 'break-all' }}>
      <Box component="span" sx={{ color: 'text.secondary', mr: 1 }}>
        <Box component="time" dateTime={new Date(entry.at).toISOString()}>
          {stamp(entry.at)}
        </Box>
      </Box>
      <Box component="span" sx={{ color: TONE[entry.level] ?? 'text.primary', mr: 1 }}>
        {entry.level}
      </Box>
      <Box component="span" sx={{ mr: 1 }}>
        {entry.target}
      </Box>
      <Box component="span" sx={{ mr: 1 }}>
        {entry.message}
      </Box>
      {entry.chain !== null && entry.network !== null && (
        <To parts={['networks', entry.chain, entry.network]}>
          {`${entry.chain}/${entry.network}`}
        </To>
      )}
      {entry.job_id !== null && (
        <To parts={['jobs', String(entry.job_id)]}>{`job ${entry.job_id}`}</To>
      )}
      {fields.length > 0 && (
        <Link
          component="button"
          underline="hover"
          color="text.secondary"
          onClick={() => setOpen(!open)}
          sx={{ fontFamily: mono, fontSize: 12, verticalAlign: 'baseline' }}
        >
          {open ? 'fold' : `${fields.length} fields`}
        </Link>
      )}
      {open && (
        <Box sx={{ color: 'text.secondary', pl: 2 }}>
          {fields.map(([key, value]) => (
            <div key={key}>{`${key}=${value}`}</div>
          ))}
        </Box>
      )}
    </Box>
  );
}
