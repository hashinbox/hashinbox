import Notes from '@mui/icons-material/Notes';
import Button from '@mui/material/Button';
import type { LogFilter } from './filter';
import { openLogs } from './open';

/** The one "Logs" button: opens the logs drawer on `filter`. Sits at the right end of a page's header. */
export function LogsButton({ filter }: { filter: LogFilter }) {
  return (
    <Button
      size="small"
      variant="outlined"
      startIcon={<Notes fontSize="small" />}
      onClick={() => openLogs(filter)}
      sx={{ flexShrink: 0 }}
    >
      Logs
    </Button>
  );
}
