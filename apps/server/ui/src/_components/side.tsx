import Box from '@mui/material/Box';
import List from '@mui/material/List';
import ListItemButton from '@mui/material/ListItemButton';
import Stack from '@mui/material/Stack';
import Typography from '@mui/material/Typography';
import type { ReactNode } from 'react';
import { mono } from '@/theme';
import { Dot } from './dot';

/**
 * A page's sidebar: `title` (with a `count`), its `actions` beside it (e.g.
 * +), `tools` under it (e.g. a filter), its items, `empty` when it has none.
 */
export function Side({
  title,
  count,
  actions,
  tools,
  empty,
  children,
}: {
  title: string;
  count?: string;
  actions?: ReactNode;
  tools?: ReactNode;
  /** Shown instead of the items when there are none. */
  empty?: ReactNode;
  children: ReactNode;
}) {
  return (
    <Box
      component="aside"
      sx={(t) => ({
        p: 2.5,
        minWidth: 0,
        // Each side named in full: a responsive border shorthand would reset its colour.
        borderBottom: `1px solid ${t.palette.divider}`,
        [t.breakpoints.up('md')]: {
          borderBottom: 0,
          borderRight: `1px solid ${t.palette.divider}`,
        },
      })}
    >
      <Stack direction="row" sx={{ alignItems: 'center', mb: 1.5, minHeight: 30 }}>
        <Typography variant="h3" sx={{ flexGrow: 1 }}>
          {title}
          {count !== undefined && (
            <Typography
              component="span"
              color="text.secondary"
              sx={{ fontFamily: mono, fontSize: 12, ml: 1 }}
            >
              {count}
            </Typography>
          )}
        </Typography>
        {actions}
      </Stack>
      {tools}
      <List disablePadding aria-label={title}>
        {children}
        {empty && (
          <Typography color="text.secondary" sx={{ py: 2, fontSize: 13 }}>
            {empty}
          </Typography>
        )}
      </List>
    </Box>
  );
}

/**
 * One sidebar card: an optional `icon` before it, `name` and its state `dot`
 * on the first line, `detail` (and an `extra` control at its end) on the
 * second, a red `problem` below.
 */
export function SideItem({
  icon,
  name,
  dot,
  detail,
  extra,
  problem,
  selected,
  onClick,
}: {
  icon?: ReactNode;
  name: string;
  dot: { color: string; label: string; live?: boolean };
  detail: ReactNode;
  extra?: ReactNode;
  problem?: ReactNode;
  selected: boolean;
  onClick: () => void;
}) {
  return (
    <ListItemButton selected={selected} onClick={onClick} sx={{ gap: 1.5, py: 1.25, px: 1 }}>
      {icon && <Box sx={{ display: 'flex', fontSize: 22 }}>{icon}</Box>}
      <Box sx={{ minWidth: 0, flexGrow: 1 }}>
        <Stack direction="row" spacing={1} sx={{ alignItems: 'center' }}>
          <Typography noWrap sx={{ fontFamily: mono, fontSize: 13, flexGrow: 1 }}>
            {name}
          </Typography>
          <Dot {...dot} />
        </Stack>
        <Stack direction="row" spacing={1} sx={{ alignItems: 'center' }}>
          <Typography noWrap sx={{ fontSize: 11, color: 'text.secondary', flexGrow: 1 }}>
            {detail}
          </Typography>
          {extra}
        </Stack>
        {problem && (
          <Typography sx={{ fontFamily: mono, fontSize: 11, color: 'error.main' }}>
            {problem}
          </Typography>
        )}
      </Box>
    </ListItemButton>
  );
}
