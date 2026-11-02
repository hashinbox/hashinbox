import Box from '@mui/material/Box';
import Typography from '@mui/material/Typography';
import type { ReactNode } from 'react';

/** Nothing picked yet: an icon, a title and a line or two on what picking one shows, centred and muted. */
export function Empty({
  icon,
  title,
  children,
}: {
  icon: ReactNode;
  title: string;
  children: ReactNode;
}) {
  return (
    <Box
      sx={{
        height: '100%',
        minHeight: 240,
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
        justifyContent: 'center',
        textAlign: 'center',
        gap: 1,
        color: 'text.disabled',
      }}
    >
      <Box sx={{ display: 'flex', '& svg': { fontSize: 40 } }}>{icon}</Box>
      <Typography sx={{ fontSize: 15, fontWeight: 600, color: 'text.secondary' }}>
        {title}
      </Typography>
      <Typography sx={{ fontSize: 13, maxWidth: 360, lineHeight: 1.5 }}>{children}</Typography>
    </Box>
  );
}
