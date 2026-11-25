import Box from '@mui/material/Box';

/**
 * A list row's state as a dot, at the right end of its first line: `color`
 * (a theme color, e.g. `success.main`), named `label` on hover; `live`, it
 * pulses.
 */
export function Dot({
  color,
  label,
  live = false,
}: {
  color: string;
  label: string;
  live?: boolean;
}) {
  return (
    <Box sx={{ width: 20, mr: -0.5, display: 'flex', justifyContent: 'center', flex: 'none' }}>
      <Box
        component="span"
        role="img"
        aria-label={label}
        title={label}
        sx={{
          position: 'relative',
          width: 8,
          height: 8,
          borderRadius: '50%',
          flex: 'none',
          bgcolor: color,
          '@keyframes pulse': {
            from: { transform: 'scale(1)', opacity: 0.6 },
            to: { transform: 'scale(2.6)', opacity: 0 },
          },
          '&::after': live
            ? {
                content: '""',
                position: 'absolute',
                inset: 0,
                borderRadius: '50%',
                bgcolor: 'inherit',
                animation: 'pulse 2s ease-out infinite',
                '@media (prefers-reduced-motion: reduce)': { animation: 'none' },
              }
            : undefined,
        }}
      />
    </Box>
  );
}
