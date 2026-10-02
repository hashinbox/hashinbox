import CssBaseline from '@mui/material/CssBaseline';
import { ThemeProvider } from '@mui/material/styles';
import useMediaQuery from '@mui/material/useMediaQuery';
import { useMemo } from 'react';
import { Layout } from '@/layout';
import { theme } from '@/theme';

/** The app under a theme that follows the system's light or dark setting. */
export function Root() {
  const dark = useMediaQuery('(prefers-color-scheme: dark)');
  const current = useMemo(() => theme(dark ? 'dark' : 'light'), [dark]);
  return (
    <ThemeProvider theme={current}>
      <CssBaseline />
      <Layout />
    </ThemeProvider>
  );
}
