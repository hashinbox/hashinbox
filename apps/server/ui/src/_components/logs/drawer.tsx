import Close from '@mui/icons-material/Close';
import Drawer from '@mui/material/Drawer';
import IconButton from '@mui/material/IconButton';
import { useEffect, useState } from 'react';
import type { Shell } from '@/layout';
import type { LogSource } from './entries';
import type { LogFilter } from './filter';
import { Logs } from './index';
import { onOpenLogs } from './open';

/** The logs of whatever `openLogs` was given, from the right, live; mounted once. */
export function LogsDrawer({ shell, source }: { shell: Shell; source: LogSource }) {
  const [preset, setPreset] = useState<LogFilter | null>(null);
  useEffect(() => {
    onOpenLogs(setPreset);
    return () => onOpenLogs(undefined);
  }, []);
  return (
    <Drawer
      anchor="right"
      open={preset !== null}
      onClose={() => setPreset(null)}
      slotProps={{
        backdrop: { sx: { bgcolor: 'rgba(0, 0, 0, 0.6)' } },
        paper: {
          elevation: 24,
          sx: {
            width: 'min(1400px, 100vw)',
            p: 2,
            gap: 1,
            borderLeft: 1,
            borderColor: 'divider',
          },
        },
      }}
    >
      <IconButton
        aria-label="Close logs"
        onClick={() => setPreset(null)}
        sx={{ alignSelf: 'flex-end' }}
      >
        <Close fontSize="small" />
      </IconButton>
      {preset && (
        <Logs key={JSON.stringify(preset)} shell={shell} source={source} preset={preset} />
      )}
    </Drawer>
  );
}
