import Box from '@mui/material/Box';
import Typography from '@mui/material/Typography';
import type { ReactNode } from 'react';

/** A stat card: a label, the value, an optional note and an optional trend under it. */
export function Stat({
  label,
  value,
  note,
  trend,
}: {
  label: string;
  value: ReactNode;
  note?: ReactNode;
  trend?: ReactNode;
}) {
  return (
    <Box
      component="section"
      aria-label={label}
      sx={{
        p: 2,
        border: 1,
        borderColor: 'divider',
        borderRadius: '8px',
        // A solid fill, the theme's card surface: the dashboard watermark stays behind it.
        bgcolor: 'background.paper',
        minWidth: 0,
        display: 'flex',
        flexDirection: 'column',
        gap: 0.5,
      }}
    >
      <Typography sx={{ fontSize: 12, color: 'text.secondary' }}>{label}</Typography>
      <Typography sx={{ fontSize: 26, fontWeight: 600, lineHeight: 1.2 }}>{value}</Typography>
      {note && <Typography sx={{ fontSize: 12, color: 'text.secondary' }}>{note}</Typography>}
      {trend && <Box sx={{ mt: 'auto', pt: 1 }}>{trend}</Box>}
    </Box>
  );
}
