import HubOutlined from '@mui/icons-material/HubOutlined';
import Badge from '@mui/material/Badge';
import Box from '@mui/material/Box';
import Divider from '@mui/material/Divider';
import IconButton from '@mui/material/IconButton';
import Popover from '@mui/material/Popover';
import Stack from '@mui/material/Stack';
import Typography from '@mui/material/Typography';
import { useState } from 'react';
import { mono } from '@/theme';

/** One network as the popover lists it. */
export interface Line {
  name: string;
  state: string;
  height: number | null;
  tip: number | null;
  /** The indexer's last failure, cleared by the next kept block. */
  error: string | null;
}

const TONE: Record<string, string> = { ready: 'success.main', quarantined: 'error.main' };
const figures = { fontFamily: mono, fontSize: 11.5, fontVariantNumeric: 'tabular-nums' } as const;

function Dot({ color }: { color: string }) {
  return (
    <Box
      component="span"
      sx={{ width: 7, height: 7, borderRadius: '50%', bgcolor: color, flex: 'none' }}
    />
  );
}

/** An icon in the top bar; opens the live state of every network and its RPC endpoints. */
export function Networks({ lines, ready }: { lines: Line[]; ready: boolean | null }) {
  const [anchor, setAnchor] = useState<HTMLElement | null>(null);
  const failing = lines.filter((l) => l.state !== 'ready').length;

  return (
    <>
      <IconButton
        aria-label="Networks"
        onClick={(e) => setAnchor(e.currentTarget)}
        sx={{ border: 1, borderColor: 'divider', borderRadius: '999px' }}
        size="small"
      >
        <Badge
          variant="dot"
          color={failing > 0 || ready === false ? 'error' : 'success'}
          invisible={ready === null}
        >
          <HubOutlined fontSize="small" />
        </Badge>
      </IconButton>
      <Popover
        open={anchor !== null}
        anchorEl={anchor}
        onClose={() => setAnchor(null)}
        anchorOrigin={{ vertical: 'bottom', horizontal: 'right' }}
        transformOrigin={{ vertical: 'top', horizontal: 'right' }}
        slotProps={{ paper: { sx: { mt: 1, width: 360, border: 1, borderColor: 'divider' } } }}
      >
        <Box sx={{ px: 2, py: 1.5 }}>
          <Typography variant="overline">
            {ready === null ? 'Connecting' : ready ? 'All networks settled' : 'Not ready'}
          </Typography>
        </Box>
        <Divider />
        {lines.length === 0 && (
          <Typography color="text.secondary" sx={{ px: 2, py: 2, fontSize: 13 }}>
            No network is reporting.
          </Typography>
        )}
        {lines.map((l, i) => (
          <Box key={l.name}>
            {i > 0 && <Divider />}
            <Stack spacing={1} sx={{ px: 2, py: 1.5 }}>
              <Stack direction="row" spacing={1} sx={{ alignItems: 'center' }}>
                <Dot color={TONE[l.state] ?? 'warning.main'} />
                <Typography sx={{ ...figures, flexGrow: 1 }}>{l.name}</Typography>
                <Typography sx={{ ...figures, color: 'text.secondary' }}>{l.state}</Typography>
              </Stack>
              <Typography sx={{ ...figures, color: 'text.secondary', pl: 2 }}>
                kept #{l.height ?? '—'} · tip #{l.tip ?? '—'}
              </Typography>
              {l.error && (
                <Typography sx={{ ...figures, color: 'error.main', pl: 2 }}>{l.error}</Typography>
              )}
            </Stack>
          </Box>
        ))}
      </Popover>
    </>
  );
}
