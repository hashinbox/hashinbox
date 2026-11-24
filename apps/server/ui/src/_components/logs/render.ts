// How an entry reads in a row: its level's tone and a short summary of its
// fields, by its event (`target`). An event without a summary of its own
// reads as its message and its own fields.
import { format } from '@/_components/format';
import { shorten } from '@/_components/trim';
import type { Shell } from '@/layout';
import { mono } from '@/theme';
import type { Entry } from './entries';

type Listings = Shell['listings'];

/** An event's summary of one of its entries. */
type Summary = (entry: Entry, listings: Listings) => string;

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

const PLUS = new Set(['received', 'minted', 'reward']);
const MINUS = new Set(['spent', 'burned', 'fee']);

/** Whether a field holds something: present, and not `-` (none). */
function given(value: string | undefined): value is string {
  return value !== undefined && value !== '' && value !== '-';
}

/** The parts that hold something, joined by ` · `. */
function line(...parts: (string | false | undefined)[]): string {
  return parts.filter(Boolean).join(' · ');
}

/** Milliseconds as `310 ms` or `5.0 s`. */
export function duration(ms: string): string {
  const n = Number(ms);
  return n < 1000 ? `${n} ms` : `${(n / 1000).toFixed(1)} s`;
}

/** Whether `entry` is a node call that failed: one logged at warn or error. */
export function failedCall(entry: Entry): boolean {
  // A failure the server expects (a node not at that block yet) is logged
  // at debug: not a failed call.
  return entry.target === 'rpc.http' && (entry.level === 'warn' || entry.level === 'error');
}

/** Whether `entry` is a node call: done (`rpc.http`) or still pending (`rpc.slow`). */
export function isCall(entry: Entry): boolean {
  return entry.target === 'rpc.http' || entry.target === 'rpc.slow';
}

/** The listing of `asset` on `entry`'s network. */
function listingOf(entry: Entry, listings: Listings, asset: string | undefined) {
  return listings.find(
    (l) => l.asset === asset && l.network.chain === entry.chain && l.network.name === entry.network,
  );
}

/** `asset`'s ticker, or its id trimmed. */
function assetName(entry: Entry, listings: Listings, asset: string | undefined): string {
  return listingOf(entry, listings, asset)?.metadata.ticker ?? shorten(asset ?? '');
}

/** `atomic` units of `asset` in display units with its ticker; as is with its id when unlisted. */
function amountOf(entry: Entry, listings: Listings, asset: string | undefined, atomic: string) {
  const meta = listingOf(entry, listings, asset)?.metadata;
  if (meta === undefined) return `${atomic} ${shorten(asset ?? '')}`;
  const text = format(atomic, { decimals: meta.decimals, display: meta.display_decimals });
  return `${text} ${meta.ticker ?? shorten(asset ?? '')}`;
}

/** A wallet's address, trimmed: a wallet id ends in it (`chain.network.address`). */
export function walletOf(wallet: string): string {
  return shorten(wallet.split('.').pop() ?? wallet);
}

/** The node a call went to: its host and stored id, or just its id. */
function nodeOf(f: Entry['fields']): string | false {
  if (given(f.endpoint)) return given(f.node) ? `${f.endpoint} #${f.node}` : f.endpoint;
  return given(f.node) && `node ${f.node}`;
}

const rpcHttp: Summary = ({ level, fields: f }) =>
  line(
    f.method,
    nodeOf(f),
    given(f.ms) && `${duration(f.ms)}${given(f.wait_ms) ? ` (wait ${duration(f.wait_ms)})` : ''}`,
    given(f.status) && f.status,
    // At debug, an error object is one the server expects: the node is not there yet.
    given(f.rpc_errors) && (level === 'debug' ? `not yet (${f.rpc_errors})` : f.rpc_errors),
    given(f.error) && f.error,
    given(f.purpose) && `for ${f.purpose}`,
  );

const rpcSlow: Summary = ({ fields: f }) =>
  line(f.method, nodeOf(f), given(f.waited_ms) && `pending ${duration(f.waited_ms)}`);

const indexLeg: Summary = (entry, listings) => {
  const f = entry.fields;
  const plus = PLUS.has(f.kind ?? '');
  const sign = plus ? '+' : MINUS.has(f.kind ?? '') ? '-' : '';
  const amount = given(f.amount) ? amountOf(entry, listings, f.asset, f.amount) : '';
  return line(
    `${sign}${amount}${given(f.wallet) ? ` ${plus ? 'to' : 'from'} ${walletOf(f.wallet)}` : ''}`,
    given(f.txid) && `tx ${shorten(f.txid)}`,
    given(f.position) && `#${f.position}`,
  );
};

const indexStep: Summary = ({ fields: f }) =>
  line(
    `${f.start ?? ''}–${f.end ?? ''}`,
    given(f.blocks) && `${f.blocks} blocks`,
    given(f.matched) && `${f.matched} matched`,
    given(f.ms) && duration(f.ms),
    given(f.calls) && `${f.calls} calls`,
  );

const indexBlock: Summary = ({ fields: f }) =>
  line(
    `#${f.position ?? ''}`,
    given(f.transactions) && `${f.transactions} txs`,
    given(f.matched) && `${f.matched} matched`,
    given(f.legs) && `${f.legs} legs`,
  );

const indexFound: Summary = ({ fields: f }) =>
  `tx ${shorten(f.txid ?? '')} ${f.reverted === 'true' ? 'reverted' : 'found'} at #${f.position ?? ''}`;

const indexSettle: Summary = (entry, listings) => {
  const f = entry.fields;
  const by = f.adjustment !== '0' && given(f.adjustment) ? f.adjustment : undefined;
  return line(
    `${walletOf(f.wallet ?? '')}/${assetName(entry, listings, f.asset)} at #${f.position ?? ''}`,
    by === undefined ? 'no change' : `adjusted by ${amountOf(entry, listings, f.asset, by)}`,
  );
};

const sendEnd: Summary = ({ fields: f }) =>
  line(f.outcome, given(f.versions) && `${f.versions} versions`, given(f.ms) && duration(f.ms));

const jobRun: Summary = ({ fields: f }) =>
  given(f.reason) ? `${f.ack ?? ''}: ${f.reason}` : line(f.ack, given(f.ms) && duration(f.ms));

const nodeDemoted: Summary = ({ fields: f }) =>
  `node ${f.node ?? ''} demoted: ${f.reason ?? ''} (streak ${f.streak ?? ''})`;

/** An event without a summary of its own: its message and its own fields as `key=val`. */
const fallback: Summary = ({ message, fields }) => {
  const own = Object.entries(fields).filter(([key]) => !SPAN.has(key));
  return [message, ...own.map(([key, value]) => `${key}=${value}`)].filter(Boolean).join(' ');
};

const SUMMARIES: Partial<Record<string, Summary>> = {
  'rpc.http': rpcHttp,
  'rpc.slow': rpcSlow,
  'index.leg': indexLeg,
  'index.step': indexStep,
  'index.block': indexBlock,
  'index.found': indexFound,
  'index.settle': indexSettle,
  'send.end': sendEnd,
  'job.run': jobRun,
  'node.demoted': nodeDemoted,
};

/** `entry` in short, by its event. */
export function summarize(entry: Entry, listings: Listings): string {
  return (SUMMARIES[entry.target] ?? fallback)(entry, listings);
}
