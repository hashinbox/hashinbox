import Box from '@mui/material/Box';
import List from '@mui/material/List';
import ListItemButton from '@mui/material/ListItemButton';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useState } from 'react';
import { mono } from '@/theme';
import type { Document, Group } from './spec';
import { Verb } from './verb';

interface Props {
  document: Document;
  groups: Group[];
  /** The selected operation's id, or `null` for the overview. */
  selected: string | null;
  onSelect: (id: string | null) => void;
}

/** The overview, then every operation under its tag, filterable by path or summary. */
export function Rail({ document, groups, selected, onSelect }: Props) {
  const [query, setQuery] = useState('');
  const needle = query.trim().toLowerCase();
  const shown = groups
    .map((g) => ({
      ...g,
      endpoints: g.endpoints.filter(
        (e) =>
          needle === '' ||
          e.path.toLowerCase().includes(needle) ||
          (e.operation.summary ?? '').toLowerCase().includes(needle) ||
          e.method.includes(needle),
      ),
    }))
    .filter((g) => g.endpoints.length > 0);

  return (
    <Box
      component="aside"
      sx={(t) => ({
        p: 2.5,
        minWidth: 0,
        borderBottom: `1px solid ${t.palette.divider}`,
        [t.breakpoints.up('md')]: {
          borderBottom: 0,
          borderRight: `1px solid ${t.palette.divider}`,
          position: 'sticky',
          top: 61,
          alignSelf: 'start',
          maxHeight: 'calc(100vh - 61px)',
          overflowY: 'auto',
        },
      })}
    >
      <Typography variant="h3">
        {document.info.title}{' '}
        <Typography component="span" color="text.secondary" sx={{ fontFamily: mono, fontSize: 12 }}>
          v{document.info.version}
        </Typography>
      </Typography>
      <TextField
        placeholder="filter operations"
        value={query}
        onChange={(e) => setQuery(e.target.value)}
        sx={{ mt: 1.5, mb: 2 }}
        slotProps={{ htmlInput: { 'aria-label': 'Filter operations', spellCheck: false } }}
      />
      <List disablePadding aria-label="Operations" sx={{ borderBottom: 1, borderColor: 'divider' }}>
        <ListItemButton
          selected={selected === null}
          onClick={() => onSelect(null)}
          sx={{ py: 1, px: 1 }}
        >
          <Typography sx={{ fontSize: 13 }}>Overview</Typography>
        </ListItemButton>
        {shown.map((group) => (
          <Box key={group.tag}>
            <Typography
              variant="overline"
              component="p"
              sx={{ pt: 2, pb: 0.5, px: 1, borderTop: 1, borderColor: 'divider' }}
            >
              {group.tag}
            </Typography>
            {group.endpoints.map((e) => (
              <ListItemButton
                key={e.id}
                selected={e.id === selected}
                onClick={() => onSelect(e.id)}
                sx={{ gap: 1, py: 0.75, px: 1, alignItems: 'baseline' }}
              >
                <Verb method={e.method} />
                <Box sx={{ minWidth: 0 }}>
                  <Typography noWrap sx={{ fontFamily: mono, fontSize: 12 }}>
                    {e.path}
                  </Typography>
                  {e.operation.summary && (
                    <Typography noWrap sx={{ fontSize: 11, color: 'text.secondary' }}>
                      {e.operation.summary}
                    </Typography>
                  )}
                </Box>
              </ListItemButton>
            ))}
          </Box>
        ))}
        {shown.length === 0 && (
          <Typography
            color="text.secondary"
            sx={{ py: 2, px: 1, fontSize: 13, borderTop: 1, borderColor: 'divider' }}
          >
            Nothing matches.
          </Typography>
        )}
      </List>
    </Box>
  );
}
