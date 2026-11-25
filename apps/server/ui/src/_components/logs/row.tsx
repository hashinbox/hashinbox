import Chip from '@mui/material/Chip';
import TableBody from '@mui/material/TableBody';
import TableCell from '@mui/material/TableCell';
import TableHead from '@mui/material/TableHead';
import TableRow from '@mui/material/TableRow';
import { useState, type ReactNode, type Ref } from 'react';
import type { Row } from './entries';
import { tagOf, type Filter, type Group } from './filter';
import { cell, COLUMNS, failedCall, figures, summarize, TONE, type Messages } from './render';

/** The columns' widths; the summary takes the rest. */
const WIDTHS = [180, 64, 170, 150, 260, undefined];
const HEADS = ['Time', 'Level', 'Event', 'Network', 'Of', 'Summary'];

export function Head() {
  return (
    <TableHead>
      <TableRow>
        {HEADS.map((head, i) => (
          <TableCell key={head} sx={{ width: WIDTHS[i] }}>
            {head}
          </TableCell>
        ))}
      </TableRow>
    </TableHead>
  );
}

/** Empty space standing for the items not rendered. */
export function Pad({ px }: { px: number }) {
  if (px <= 0) return null;
  return (
    <TableBody>
      <TableRow sx={{ height: px }}>
        <TableCell colSpan={COLUMNS} sx={{ p: 0, border: 0 }} />
      </TableRow>
    </TableBody>
  );
}

/** One rendered item of the list: measured, by its `key`. */
export function Item({
  itemKey,
  measure,
  children,
}: {
  itemKey: string;
  measure: Ref<HTMLTableSectionElement>;
  children: ReactNode;
}) {
  return (
    <TableBody ref={measure} data-key={itemKey}>
      {children}
    </TableBody>
  );
}

/** `local` `YYYY-MM-DD HH:MM:SS.mmm` of `at` (Unix ms). */
function stamp(at: number): string {
  const d = new Date(at);
  const two = (n: number) => String(n).padStart(2, '0');
  const date = `${d.getFullYear()}-${two(d.getMonth() + 1)}-${two(d.getDate())}`;
  const time = `${two(d.getHours())}:${two(d.getMinutes())}:${two(d.getSeconds())}`;
  return `${date} ${time}.${String(d.getMilliseconds()).padStart(3, '0')}`;
}

/** A tag of the entry: clicked, it filters by it. */
function TagChip({
  group,
  value,
  color,
  onTag,
}: {
  group: Group;
  value: string | number | null | undefined;
  color?: string;
  onTag: (tag: Filter) => void;
}) {
  if (value === null || value === undefined) return null;
  const tag = tagOf(group, String(value));
  return (
    <Chip
      size="small"
      label={tag.label}
      title={String(value)}
      onClick={(event) => {
        event.stopPropagation();
        onTag(tag);
      }}
      sx={{
        height: 20,
        maxWidth: '100%',
        fontSize: 11,
        fontFamily: figures.fontFamily,
        borderRadius: '6px',
        bgcolor: 'action.selected',
        color,
      }}
    />
  );
}

/** One entry as one row; clicked, its fields fold out below as `key=val`. */
export function LogRow({
  row,
  messages,
  onTag,
}: {
  row: Row;
  messages: Messages;
  onTag: (tag: Filter) => void;
}) {
  const [open, setOpen] = useState(false);
  const { entry } = row;
  const network =
    entry.chain !== null && entry.network !== null && `${entry.chain}/${entry.network}`;
  const fields = Object.entries(entry.fields);
  return (
    <>
      <TableRow
        hover
        onClick={() => setOpen(!open)}
        sx={{
          cursor: 'pointer',
          '@keyframes fresh': { from: { backgroundColor: 'rgba(255, 196, 0, 0.25)' } },
          ...(row.fresh && { animation: 'fresh 2s ease-out' }),
        }}
      >
        <TableCell sx={{ ...cell, color: 'text.secondary' }}>
          <time dateTime={new Date(entry.at).toISOString()}>{stamp(entry.at)}</time>
        </TableCell>
        <TableCell
          sx={{ ...cell, color: TONE[entry.level] ?? 'text.primary', cursor: 'pointer' }}
          onClick={(event) => {
            event.stopPropagation();
            onTag(tagOf('level', entry.level));
          }}
        >
          {entry.level}
        </TableCell>
        <TableCell sx={cell}>
          <TagChip group="target" value={entry.target} onTag={onTag} />
        </TableCell>
        <TableCell sx={cell}>
          <TagChip group="network" value={network || null} onTag={onTag} />
        </TableCell>
        {/* What it is of: its job, wallet, transaction and trigger, those it has. */}
        <TableCell sx={{ ...cell, '& > *:not(:last-child)': { mr: 0.5 } }}>
          <TagChip group="job_id" value={entry.job_id} onTag={onTag} />
          <TagChip group="wallet_id" value={entry.wallet_id} onTag={onTag} />
          <TagChip group="transaction_id" value={entry.transaction_id} onTag={onTag} />
          <TagChip group="trigger_id" value={entry.trigger_id} onTag={onTag} />
        </TableCell>
        <TableCell sx={{ ...cell, color: failedCall(entry) ? 'error.main' : undefined }}>
          {summarize(entry, messages)}
        </TableCell>
      </TableRow>
      {open && fields.length > 0 && (
        <TableRow>
          <TableCell
            colSpan={COLUMNS}
            sx={{ ...figures, color: 'text.secondary', wordBreak: 'break-all', pl: 4 }}
          >
            {fields.map(([key, value]) => (
              <div key={key}>{`${key}=${value}`}</div>
            ))}
          </TableCell>
        </TableRow>
      )}
    </>
  );
}
