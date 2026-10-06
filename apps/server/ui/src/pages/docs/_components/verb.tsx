import Box from '@mui/material/Box';
import { mono } from '@/theme';

/** Reads change nothing; writes are the ones to look at twice. */
const TONE: Record<string, string> = {
  get: 'success.main',
  post: 'warning.main',
  put: 'warning.main',
  patch: 'warning.main',
  delete: 'error.main',
};

/** An HTTP method as a fixed-width mono tag. */
export function Verb({ method, wide = false }: { method: string; wide?: boolean }) {
  return (
    <Box
      component="span"
      sx={{
        fontFamily: mono,
        fontSize: wide ? 11.5 : 10,
        fontWeight: 500,
        letterSpacing: '.08em',
        textTransform: 'uppercase',
        color: TONE[method] ?? 'text.secondary',
        minWidth: wide ? 0 : 38,
        flex: 'none',
      }}
    >
      {method}
    </Box>
  );
}
