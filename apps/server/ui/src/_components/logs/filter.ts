// The log view's filter: tags, each named by the query parameter it sets,
// and a time jump. A preset (`openLogs`) becomes its first tags.
import type { Tag } from '@/_components/tag';
import { shorten } from '@/_components/trim';
import type { Query } from './entries';
import { walletOf } from './render';

/** The query parameters a tag sets. */
export type Group =
  'level' | 'target' | 'network' | 'wallet_id' | 'job_id' | 'transaction_id' | 'trigger_id';

export type Filter = Tag<Group>;

/** What the log view is opened on: a network (`chain/name`), job, wallet, transaction or trigger. */
export type LogFilter = Partial<
  Record<'network' | 'job_id' | 'wallet_id' | 'transaction_id' | 'trigger_id', string | number>
>;

export const LEVELS = ['error', 'warn', 'info', 'debug', 'trace'] as const;

/** The level tag's value for exactly errors (`level=error&only=true`). */
const ERRORS_ONLY = 'error only';

/** How a tag of `group` reads. */
function labelOf(group: Group, value: string): string {
  if (group === 'level') return value === ERRORS_ONLY ? 'errors only' : `${value} and above`;
  if (group === 'wallet_id') return walletOf(value);
  if (group === 'transaction_id') return shorten(value.split('.').pop() ?? value);
  return value;
}

export function tagOf(group: Group, value: string): Filter {
  return { group, value, label: labelOf(group, value) };
}

/** The level choices: errors only, then each level and above. */
export function levelTags(): Filter[] {
  return [tagOf('level', ERRORS_ONLY), ...LEVELS.map((l) => tagOf('level', l))];
}

/** `preset`'s tags, and a level: info and above, debug for a job (so its node calls show). */
export function tagsOf(preset: LogFilter): Filter[] {
  const given = Object.entries(preset).map(([group, value]) =>
    tagOf(group as Group, String(value)),
  );
  return [tagOf('level', preset.job_id === undefined ? 'info' : 'debug'), ...given];
}

/** `tags` with `tag` in place of its group's. */
export function withTag(tags: Filter[], tag: Filter): Filter[] {
  return [...tags.filter((t) => t.group !== tag.group), tag];
}

/** The query `tags` and the time jump `jump` (local `YYYY-MM-DDTHH:MM`, or empty) set. */
export function queryOf(tags: Filter[], jump: string): Query {
  const query: Query = {};
  for (const { group, value } of tags) query[group] = value;
  if (query.level === ERRORS_ONLY) Object.assign(query, { level: 'error', only: 'true' });
  const before = new Date(jump).getTime();
  if (jump !== '' && !Number.isNaN(before)) query.before = String(before);
  return query;
}
