// How an entry reads in a row: its level's tone and a summary, its own
// message or its event's (`target`) template filled from its fields. An event
// without a template reads as its code and its own fields.
import { shorten } from '@/_components/trim';
import { mono } from '@/theme';
import type { Entry } from './entries';

/** Each event's message template, `{key}` filled from the field `key` (`/v1/logs/messages`). */
export type Messages = Record<string, string>;

/** The columns of a log row: time, level, event, network, what it is of, summary. */
export const COLUMNS = 6;

/** A collapsed row's height, in px, until it is measured. */
export const ROW_PX = 33;

export const figures = {
  fontFamily: mono,
  fontSize: 12,
  fontVariantNumeric: 'tabular-nums',
} as const;

/** One line, cut to its cell. */
export const cell = {
  ...figures,
  whiteSpace: 'nowrap',
  overflow: 'hidden',
  textOverflow: 'ellipsis',
} as const;

export const TONE: Record<string, string> = {
  error: 'error.main',
  warn: 'warning.main',
  info: 'text.primary',
  debug: 'text.secondary',
  trace: 'text.disabled',
};

/** Fields of the spans an entry ran in, not of the event itself. */
const SPAN = new Set(['network', 'node', 'job_id', 'kind', 'topic', 'attempt', 'purpose']);

/** Milliseconds as `310 ms` or `5.0 s`. */
export function duration(ms: string): string {
  const n = Number(ms);
  return n < 1000 ? `${n} ms` : `${(n / 1000).toFixed(1)} s`;
}

/** Whether `entry` is a node call that failed or broke. */
export function failedCall(entry: Entry): boolean {
  return entry.target === 'rpc.http.failed' || entry.target === 'rpc.http.broken';
}

/** Whether `entry` is a node call: done (`rpc.http.*`) or still pending (`rpc.slow`). */
export function isCall(entry: Entry): boolean {
  return entry.target.startsWith('rpc.http.') || entry.target === 'rpc.slow';
}

/** A wallet's address, trimmed: a wallet id ends in it (`chain.network.address`). */
export function walletOf(wallet: string): string {
  return shorten(wallet.split('.').pop() ?? wallet);
}

/** `template` with each `{key}` filled from `fields`; one it lacks stays `{key}`. */
function fill(template: string, fields: Entry['fields']): string {
  return template.replace(/\{(\w+)\}/g, (whole, key: string) => fields[key] ?? whole);
}

/** `entry` in short: its own message, its event's template filled, or its code and own fields. */
export function summarize(entry: Entry, messages: Messages): string {
  if (entry.message !== null) return entry.message;
  const template = messages[entry.target];
  if (template !== undefined) return fill(template, entry.fields);
  const own = Object.entries(entry.fields).filter(([key]) => !SPAN.has(key));
  return [entry.target, ...own.map(([key, value]) => `${key}=${value}`)].join(' ');
}
