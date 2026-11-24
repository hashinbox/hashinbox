import Box from '@mui/material/Box';
import { logSource } from '@/api/logs';
import type { Shell } from '@/layout';
import { Logs as View } from '@/_components/logs';

/** Every kept log entry, newest first, and each new one live; full width, filtered. */
export function Logs({ shell }: { shell: Shell }) {
  return (
    <Box
      sx={{
        px: { xs: 2, md: 4 },
        py: { xs: 2.5, md: 3.5 },
        minWidth: 0,
        height: 'calc(100dvh - 60px)',
        display: 'flex',
        flexDirection: 'column',
      }}
    >
      <View shell={shell} source={logSource} />
    </Box>
  );
}
