// What the log list renders, item by item: an entry's row, or, for a job,
// each run's header, its node calls folded into one line, then its story.
import type { Row } from './entries';
import { isCall } from './render';

export type Item =
  | { kind: 'row'; key: string; row: Row }
  | { kind: 'run'; key: string; attempt: string; end: Row | undefined }
  | { kind: 'calls'; key: string; calls: Row[] };

/** `rows` by the run they are of (`fields.attempt`), newest first; `''` for none. */
function runsOf(rows: Row[]): Map<string, Row[]> {
  const runs = new Map<string, Row[]>();
  for (const row of rows) {
    const attempt = row.entry.fields.attempt ?? '';
    runs.set(attempt, [...(runs.get(attempt) ?? []), row]);
  }
  return runs;
}

/** One run's items: its header (from its `job.run`, the run's last event), its calls, its story. */
function runItems(attempt: string, rows: Row[]): Item[] {
  const end = rows.find((row) => row.entry.target === 'job.run');
  const calls = rows.filter((row) => isCall(row.entry));
  const story = rows.filter((row) => row !== end && !isCall(row.entry));
  return [
    { kind: 'run', key: `run-${attempt}`, attempt, end },
    ...(calls.length > 0 ? [{ kind: 'calls' as const, key: `calls-${attempt}`, calls }] : []),
    ...story.map((row) => ({ kind: 'row' as const, key: row.key, row })),
  ];
}

/** `rows` as items: one per row, or grouped by run when `byRun`. */
export function itemsOf(rows: Row[], byRun: boolean): Item[] {
  if (!byRun) return rows.map((row) => ({ kind: 'row', key: row.key, row }));
  return [...runsOf(rows)].flatMap(([attempt, run]) => runItems(attempt, run));
}
