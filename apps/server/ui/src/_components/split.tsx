import Close from '@mui/icons-material/Close';
import Box from '@mui/material/Box';
import Drawer from '@mui/material/Drawer';
import IconButton from '@mui/material/IconButton';
import { useTheme } from '@mui/material/styles';
import useMediaQuery from '@mui/material/useMediaQuery';
import type { ReactNode } from 'react';

interface Props {
  /** The list: always shown. */
  list: ReactNode;
  /** What the list has open. */
  detail: ReactNode;
  /** Whether anything is open; on a narrow screen, the drawer is shown while it is. */
  open: boolean;
  /** Closes what is open. */
  onClose: () => void;
  /** The grid's columns when both fit. */
  columns: string;
  /** From this width on, both fit side by side. */
  wide?: 'md' | 'lg';
}

/**
 * A list beside what it has open. Too narrow for both, the list fills the
 * page and what it opens comes up from the bottom over the whole screen.
 */
export function Split({ list, detail, open, onClose, columns, wide = 'md' }: Props) {
  const theme = useTheme();
  const fits = useMediaQuery(theme.breakpoints.up(wide));
  if (fits) {
    return (
      <Box sx={{ display: 'grid', gridTemplateColumns: columns, flexGrow: 1 }}>
        {list}
        {detail}
      </Box>
    );
  }
  return (
    <>
      {/* The list alone fills the page's height, as it does beside its detail. */}
      <Box sx={{ display: 'grid', gridTemplateColumns: 'minmax(0, 1fr)', flexGrow: 1 }}>{list}</Box>
      <Drawer
        anchor="bottom"
        open={open}
        onClose={onClose}
        slotProps={{ paper: { sx: { height: '100dvh', overflowX: 'hidden' } } }}
      >
        <Box
          sx={{
            position: 'sticky',
            top: 0,
            zIndex: 1,
            display: 'flex',
            justifyContent: 'flex-end',
            px: 1,
            py: 0.5,
            bgcolor: 'background.paper',
            borderBottom: 1,
            borderColor: 'divider',
          }}
        >
          <IconButton size="small" aria-label="Close" onClick={onClose}>
            <Close fontSize="small" />
          </IconButton>
        </Box>
        {detail}
      </Drawer>
    </>
  );
}
