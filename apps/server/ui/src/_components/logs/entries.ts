import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { Shell } from '@/layout';

/** One kept entry as a page or the stream holds it; a live one's id is 0. */
export interface Entry {
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
  wallet_id?: string | null;
  transaction_id?: string | null;
  trigger_id?: string | null;
}

/** One page of entries: newest first, or oldest first when read `after` a moment. */
export interface Page {
  logs: Entry[];
  next_cursor: string | null;
}

/** One event of the stream: its name and its JSON data. */
export interface Heard {
  event: string;
  data: unknown;
}

/** A filter of `/v1/logs` and its stream, as query text: `level`, `job_id`, `before`… */
export type Query = Record<string, string>;

/** Where entries come from: a page of them, the live stream, the events kept. */
export interface LogSource {
  read: (query: Query, cursor: string | undefined) => Promise<Page>;
  listen: (query: Query, onEvent: (heard: Heard) => void, signal: AbortSignal) => Promise<void>;
  targets: () => Promise<string[]>;
}

/** An entry as shown, with a key of its own (a live one has no id); `fresh` while just heard. */
export interface Row {
  key: string;
  entry: Entry;
  fresh: boolean;
}

/**
 * The rows held, newest first; whether older or newer ones are left to read
 * (newer: the rows end before now, so no live entry goes on top); and
 * whether the last change should keep the scroll where it was.
 */
export interface Held {
  rows: Row[];
  older: boolean;
  newer: boolean;
  anchored: boolean;
}

/** The most rows held; beyond them the far end goes, read again when scrolled back to. */
const MOST = 2000;
/** How long a live row stays highlighted. */
const FRESH_MS = 2000;

/** Until the first page: nothing, and no stream yet. */
const NONE: Held = { rows: [], older: false, newer: true, anchored: false };

/** Live rows so far, for their keys. */
let lived = 0;

function stored(entry: Entry): Row {
  return { key: String(entry.id), entry, fresh: false };
}

function oldestStored(rows: Row[]): Row | undefined {
  return [...rows].reverse().find((row) => row.entry.id > 0);
}

function newestStored(rows: Row[]): Row | undefined {
  return rows.find((row) => row.entry.id > 0);
}

/** An older page (newest first) below `held`; past `MOST`, the newest go. */
function below(held: Held, page: Page): Held {
  const rows = [...held.rows, ...page.logs.map(stored)];
  const cut = rows.length > MOST;
  return {
    rows: cut ? rows.slice(rows.length - MOST) : rows,
    older: page.next_cursor !== null,
    newer: held.newer || cut,
    anchored: true,
  };
}

/** `fresh` rows, newest first, on top of `held`; past `MOST`, the oldest go. */
function above(held: Held, fresh: Row[], newer: boolean, anchored: boolean): Held {
  const rows = [...fresh, ...held.rows];
  const cut = rows.length > MOST;
  return {
    rows: cut ? rows.slice(0, MOST) : rows,
    older: held.older || cut,
    newer,
    anchored,
  };
}

/** `keys` no longer highlighted. */
function settle(held: Held, keys: Set<string>): Held {
  const rows = held.rows.map((row) => (keys.has(row.key) ? { ...row, fresh: false } : row));
  return { ...held, rows };
}

/**
 * The entries `query` matches, newest first: the page before its time jump
 * (`before`), or the newest page and each new entry live. `older` and
 * `newer` read the page beyond either end. A live entry goes on top while
 * `atTop`, else it waits until `showWaiting`.
 */
export function useEntries(source: LogSource, query: Query, shell: Shell, atTop: () => boolean) {
  const [held, setHeld] = useState<Held>(NONE);
  const [waiting, setWaiting] = useState<Row[]>([]);
  const loading = useRef(true);
  const { fail, notify } = shell;
  // Newer pages and the stream run from the jump on, not before it.
  const onward = useMemo(
    () => Object.fromEntries(Object.entries(query).filter(([key]) => key !== 'before')),
    [query],
  );

  useEffect(() => {
    let current = true;
    source
      .read(query, undefined)
      .then((page) => {
        if (!current) return;
        loading.current = false;
        setHeld({ ...below(NONE, page), newer: query.before !== undefined, anchored: false });
      })
      .catch((error: unknown) => {
        if (current) fail(error);
      });
    return () => {
      current = false;
    };
  }, [source, query, fail]);

  const add = useCallback((fresh: Row[]) => {
    setHeld((h) => above(h, fresh, false, false));
    const keys = new Set(fresh.map((row) => row.key));
    window.setTimeout(() => setHeld((h) => settle(h, keys)), FRESH_MS);
  }, []);

  useEffect(() => {
    if (held.newer) return;
    const abort = new AbortController();
    source
      .listen(
        onward,
        ({ event, data }) => {
          if (event === 'missed') {
            notify(`${(data as { missed: number }).missed} log entries missed.`, true);
            return;
          }
          const row = { key: `live-${(lived += 1)}`, entry: data as Entry, fresh: true };
          if (atTop()) add([row]);
          else setWaiting((rows) => [row, ...rows].slice(0, MOST));
        },
        abort.signal,
      )
      .catch((error: unknown) => {
        if (!abort.signal.aborted) fail(error);
      });
    return () => abort.abort();
  }, [held.newer, source, onward, atTop, add, notify, fail]);

  /** Runs `read` unless a read is under way; `then` gets its page. */
  const load = useCallback(
    (read: () => Promise<Page>, then: (page: Page) => void) => {
      if (loading.current) return;
      loading.current = true;
      read()
        .then((page) => {
          loading.current = false;
          then(page);
        })
        .catch((error: unknown) => {
          loading.current = false;
          fail(error);
        });
    },
    [fail],
  );

  const older = useCallback(() => {
    if (!held.older) return;
    const last = oldestStored(held.rows);
    load(
      () => source.read(query, last && String(last.entry.id)),
      (page) => setHeld((h) => below(h, page)),
    );
  }, [held, source, query, load]);

  const newer = useCallback(() => {
    if (!held.newer) return;
    // Read oldest first from the newest held (or the jump): `cursor` is exact.
    const top = newestStored(held.rows);
    const after = top ? String(top.entry.at) : query.before;
    const page = after === undefined ? onward : { ...onward, after };
    load(
      () => source.read(page, top && String(top.entry.id)),
      (read) => {
        const done = read.next_cursor === null;
        if (done) setWaiting([]);
        setHeld((h) => above(h, read.logs.map(stored).reverse(), !done, true));
      },
    );
  }, [held, source, query.before, onward, load]);

  const showWaiting = useCallback(() => {
    add(waiting);
    setWaiting([]);
  }, [add, waiting]);

  return { held, older, newer, waiting: waiting.length, showWaiting };
}
