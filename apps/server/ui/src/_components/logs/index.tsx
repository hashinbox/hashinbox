import Button from '@mui/material/Button';
import Stack from '@mui/material/Stack';
import Table from '@mui/material/Table';
import TableBody from '@mui/material/TableBody';
import TableContainer from '@mui/material/TableContainer';
import Typography from '@mui/material/Typography';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { EmptyRow } from '@/_components/table';
import { useEntries, type LogSource } from './entries';
import { queryOf, tagsOf, withTag, type Filter, type LogFilter } from './filter';
import { Filters } from './filters';
import { itemsOf, type Item } from './items';
import { COLUMNS, ROW_PX, type Messages } from './render';
import { Head, Item as Measured, LogRow, Pad } from './row';
import { Calls, RunHead } from './runs';
import { useVirtual } from './virtual';
import { fail } from '@/store/app';

/** How near an end of the list, in px, the page beyond it is read. */
const NEAR_PX = 400;

/**
 * Kept log entries, newest first, filtered: `preset`'s tags and a level (info
 * and above) to start with. Fills its parent's height; see `View`.
 */
export function Logs({ source, preset = {} }: { source: LogSource; preset?: LogFilter }) {
  const [tags, setTags] = useState(() => tagsOf(preset));
  const [jump, setJump] = useState('');
  // Bumped by "Live": read the newest again.
  const [reads, setReads] = useState(0);
  const search = new URLSearchParams(queryOf(tags, jump)).toString();
  const live = useCallback(() => {
    setJump('');
    setReads((n) => n + 1);
  }, []);
  const addTag = useCallback((tag: Filter) => setTags((was) => withTag(was, tag)), []);
  const [messages, setMessages] = useState<Messages>({});
  useEffect(() => {
    source.messages().then(setMessages).catch(fail);
  }, [source]);
  return (
    <Stack spacing={1} sx={{ minHeight: 0, flexGrow: 1 }}>
      <Filters source={source} tags={tags} onTags={setTags} jump={jump} onJump={setJump} />
      <View
        key={`${search}#${reads}`}
        source={source}
        search={search}
        messages={messages}
        onTag={addTag}
        onLive={live}
      />
    </Stack>
  );
}

/**
 * The entries `search` matches, as a table only partly in the DOM. Scrolled
 * near the bottom, older ones load; near the top, newer ones until the live
 * head. At the top of the live head new entries go on top; scrolled away,
 * they are counted. For a job, rows group by run.
 */
function View({
  source,
  search,
  messages,
  onTag,
  onLive,
}: {
  source: LogSource;
  search: string;
  messages: Messages;
  onTag: (tag: Filter) => void;
  onLive: () => void;
}) {
  const query = useMemo(() => Object.fromEntries(new URLSearchParams(search)), [search]);
  const scroller = useRef<HTMLDivElement>(null);
  const atTop = useCallback(() => (scroller.current?.scrollTop ?? 0) < 4, []);
  const { held, older, newer, waiting, showWaiting } = useEntries(source, query, atTop);
  const items = useMemo(() => itemsOf(held.rows, query.job_id !== undefined), [held.rows, query]);
  const keys = useMemo(() => items.map((item) => item.key), [items]);
  const virtual = useVirtual(keys, held.anchored, scroller, ROW_PX);

  // Near an end, read the page beyond it; at the top, show the entries waiting.
  const nearEnds = useCallback(() => {
    const element = scroller.current;
    if (!element) return;
    if (element.scrollTop < NEAR_PX) newer();
    if (element.scrollHeight - element.scrollTop - element.clientHeight < NEAR_PX) older();
    if (atTop() && waiting > 0) showWaiting();
  }, [newer, older, atTop, waiting, showWaiting]);
  // A page read may leave the view still near an end: read on.
  useEffect(nearEnds, [nearEnds]);
  const onScroll = () => {
    virtual.onScroll();
    nearEnds();
  };
  const toTop = () => {
    scroller.current?.scrollTo({ top: 0 });
    showWaiting();
  };

  return (
    <Stack spacing={1} sx={{ flexGrow: 1, minHeight: 0 }}>
      <Stack direction="row" spacing={1.5} sx={{ alignItems: 'center', minHeight: 30 }}>
        {held.newer || query.before !== undefined ? (
          <Button size="small" onClick={onLive}>
            Live
          </Button>
        ) : (
          <Typography color="text.secondary" sx={{ fontSize: 13 }}>
            Live
          </Typography>
        )}
        {!held.newer && waiting > 0 && (
          <Button size="small" onClick={toTop}>
            {`${waiting} new ↑`}
          </Button>
        )}
      </Stack>
      <TableContainer
        ref={scroller}
        onScroll={onScroll}
        sx={{ flexGrow: 1, minHeight: 0, overflow: 'auto', overflowAnchor: 'none' }}
      >
        <Table size="small" stickyHeader sx={{ tableLayout: 'fixed', minWidth: 1100 }}>
          <Head />
          {items.length === 0 && (
            <TableBody>
              <EmptyRow columns={COLUMNS} message="No entry is kept." />
            </TableBody>
          )}
          <Pad px={virtual.before} />
          {items.slice(virtual.start, virtual.end).map((item) => (
            <Measured key={item.key} itemKey={item.key} measure={virtual.measure}>
              <Shown item={item} messages={messages} onTag={onTag} />
            </Measured>
          ))}
          <Pad px={virtual.after} />
        </Table>
      </TableContainer>
    </Stack>
  );
}

/** One item's rows: an entry, a run's header, or a run's calls. */
function Shown({
  item,
  messages,
  onTag,
}: {
  item: Item;
  messages: Messages;
  onTag: (tag: Filter) => void;
}) {
  if (item.kind === 'run') return <RunHead attempt={item.attempt} end={item.end} />;
  if (item.kind === 'calls') return <Calls calls={item.calls} />;
  return <LogRow row={item.row} messages={messages} onTag={onTag} />;
}
