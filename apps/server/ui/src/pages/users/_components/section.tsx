import Box from '@mui/material/Box';
import Stack from '@mui/material/Stack';
import Typography from '@mui/material/Typography';
import type { ReactNode } from 'react';

/** One part of a user's page: a title, its `actions` beside it, then it. */
export function Section({
  title,
  actions,
  children,
}: {
  title: string;
  actions?: ReactNode;
  children: ReactNode;
}) {
  return (
    <Box component="section" aria-label={title} sx={{ mt: 4 }}>
      <Stack
        direction="row"
        spacing={1}
        sx={{ alignItems: 'center', pb: 1, mb: 2, borderBottom: 1, borderColor: 'divider' }}
      >
        <Typography variant="h3" component="h2" sx={{ flexGrow: 1 }}>
          {title}
        </Typography>
        {actions}
      </Stack>
      {children}
    </Box>
  );
}

/** A label and its value, one row of a section. */
export function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <Stack direction={{ xs: 'column', sm: 'row' }} spacing={{ xs: 0.25, sm: 2 }} sx={{ py: 0.75 }}>
      <Typography color="text.secondary" sx={{ fontSize: 13, width: 120, flex: 'none' }}>
        {label}
      </Typography>
      <Box sx={{ fontSize: 13, minWidth: 0 }}>{children}</Box>
    </Stack>
  );
}
