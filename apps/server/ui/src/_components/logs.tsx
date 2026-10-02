import Box from '@mui/material/Box';
import Chip from '@mui/material/Chip';
import FormControlLabel from '@mui/material/FormControlLabel';
import Stack from '@mui/material/Stack';
import Switch from '@mui/material/Switch';
import { useEffect, useRef, useState, type ReactNode } from 'react';
import { mono } from '@/theme';

export type Level = 'info' | 'warn' | 'error';

export interface Entry {
  id: number;
  at: Date;
  level: Level;
  source: string | null;
  text: string;
}

const TONE: Record<Level, string> = {
  info: 'text.secondary',
  warn: 'warning.main',
  error: 'error.main',
};
const LEVELS: (Level | 'all')[] = ['all', 'warn', 'error'];

function clock(at: Date): string {
  return at.toTimeString().slice(0, 8);
}

/** A tailing log: newest line at the bottom, filterable by severity. */
export function Logs({
  entries,
  height = 320,
  controls,
}: {
  entries: Entry[];
  height?: number;
  controls?: ReactNode;
}) {
  const [level, setLevel] = useState<Level | 'all'>('all');
  const [follow, setFollow] = useState(true);
  const pane = useRef<HTMLDivElement>(null);

  const shown = entries.filter(
    (e) => level === 'all' || e.level === level || (level === 'warn' && e.level === 'error'),
  );

  useEffect(() => {
    // Scroll the pane itself, never the page around it.
    const box = pane.current;
    if (follow && box) box.scrollTop = box.scrollHeight;
  }, [shown.length, follow]);

  return (
    <Box sx={{ border: 1, borderColor: 'divider' }}>
      <Stack
        direction="row"
        spacing={0.75}
        sx={{
          alignItems: 'center',
          flexWrap: 'wrap',
          gap: 1,
          px: 1.5,
          py: 1,
          borderBottom: 1,
          borderColor: 'divider',
        }}
      >
        {LEVELS.map((l) => (
          <Chip
            key={l}
            size="small"
            label={l.toUpperCase()}
            variant={level === l ? 'filled' : 'outlined'}
            onClick={() => setLevel(l)}
          />
        ))}
        <Box sx={{ flexGrow: 1 }} />
        {controls}
        <FormControlLabel
          control={
            <Switch size="small" checked={follow} onChange={(e) => setFollow(e.target.checked)} />
          }
          label="Follow"
          slotProps={{ typography: { sx: { fontSize: 12, color: 'text.secondary' } } }}
          sx={{ mr: 0 }}
        />
      </Stack>
      <Box
        ref={pane}
        role="log"
        aria-live="off"
        sx={{
          height,
          overflowY: 'auto',
          px: 1.5,
          py: 1,
          fontFamily: mono,
          fontSize: 12,
          lineHeight: 1.75,
        }}
      >
        {shown.map((e) => (
          <Box key={e.id} sx={{ display: 'flex', gap: 1.5, whiteSpace: 'nowrap' }}>
            <Box component="span" sx={{ color: 'text.disabled' }}>
              {clock(e.at)}
            </Box>
            <Box component="span" sx={{ color: TONE[e.level], width: 40, flex: 'none' }}>
              {e.level.toUpperCase()}
            </Box>
            {e.source && (
              <Box component="span" sx={{ color: 'text.secondary' }}>
                [{e.source}]
              </Box>
            )}
            <Box component="span" sx={{ whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' }}>
              {e.text}
            </Box>
          </Box>
        ))}
      </Box>
    </Box>
  );
}
