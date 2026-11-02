import Box from '@mui/material/Box';
import { span, useNow } from './clock';

/**
 * A moment (Unix ms) as a live count: `in 3m 05s` counting down, `12s ago`
 * counting up, `now` within a second. Its local date and time is the tooltip.
 * `due`: a moment something is due, so once passed it reads `now`, never `ago`.
 */
export function When({ at, due = false }: { at: number; due?: boolean }) {
  const now = useNow();
  const ahead = at - now;
  const text =
    Math.abs(ahead) < 1000 || (due && ahead < 0)
      ? 'now'
      : ahead > 0
        ? `in ${span(ahead)}`
        : `${span(ahead)} ago`;
  return (
    <Box
      component="time"
      dateTime={new Date(at).toISOString()}
      title={new Date(at).toLocaleString()}
      sx={{ fontVariantNumeric: 'tabular-nums', whiteSpace: 'nowrap' }}
    >
      {text}
    </Box>
  );
}
