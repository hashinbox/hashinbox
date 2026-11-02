import Add from '@mui/icons-material/Add';
import Box from '@mui/material/Box';
import IconButton from '@mui/material/IconButton';
import List from '@mui/material/List';
import ListItemButton from '@mui/material/ListItemButton';
import Stack from '@mui/material/Stack';
import Tooltip from '@mui/material/Tooltip';
import Typography from '@mui/material/Typography';
import { useCallback, useEffect, useState } from 'react';
import { call, client, type ConfiguredNetwork } from '@/api/client';
import type { Shell } from '@/layout';
import { mono } from '@/theme';
import { Nodes } from './_components/nodes';
import { AddNodes } from './_components/add';

const BEAT_MS = 4000;

/** `chain/network`. */
function nameOf(n: ConfiguredNetwork): string {
  return `${n.network.chain}/${n.network.name}`;
}

/** The networks beside the one whose RPC nodes are shown. */
export function Networks({ shell }: { shell: Shell }) {
  const [networks, setNetworks] = useState<ConfiguredNetwork[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);

  const refresh = useCallback(() => {
    call(client.GET('/v1/networks'))
      .then((list) => setNetworks(list.networks))
      .catch(shell.fail);
  }, [shell.fail]);

  useEffect(() => {
    refresh();
    const timer = window.setInterval(refresh, BEAT_MS);
    return () => window.clearInterval(timer);
  }, [refresh]);

  const current = networks.find((n) => nameOf(n) === selected) ?? networks[0] ?? null;

  return (
    <Box
      sx={{
        display: 'grid',
        gridTemplateColumns: { xs: 'minmax(0, 1fr)', md: 'minmax(220px, 280px) minmax(0, 1fr)' },
        minHeight: 'calc(100vh - 65px)',
      }}
    >
      <Box
        component="aside"
        sx={(t) => ({
          p: 2.5,
          minWidth: 0,
          borderBottom: `1px solid ${t.palette.divider}`,
          [t.breakpoints.up('md')]: {
            borderBottom: 0,
            borderRight: `1px solid ${t.palette.divider}`,
          },
        })}
      >
        <Stack direction="row" sx={{ alignItems: 'center', mb: 1.5 }}>
          <Typography variant="h3" sx={{ flexGrow: 1 }}>
            Networks
          </Typography>
          <Tooltip title="Add a provider to every network">
            <IconButton size="small" aria-label="Add RPC nodes" onClick={() => setAdding(true)}>
              <Add fontSize="small" />
            </IconButton>
          </Tooltip>
        </Stack>
        <List disablePadding aria-label="Networks">
          {networks.map((n) => (
            <ListItemButton
              key={nameOf(n)}
              selected={current !== null && nameOf(n) === nameOf(current)}
              onClick={() => setSelected(nameOf(n))}
              sx={{ py: 1.25, px: 1, display: 'block' }}
            >
              <Stack direction="row" spacing={1} sx={{ alignItems: 'center' }}>
                <Typography noWrap sx={{ fontFamily: mono, fontSize: 13, flexGrow: 1 }}>
                  {nameOf(n)}
                </Typography>
                <Box
                  component="span"
                  aria-label={n.indexing.error ? 'retrying' : 'ready'}
                  sx={{
                    width: 8,
                    height: 8,
                    borderRadius: '50%',
                    flex: 'none',
                    bgcolor: n.indexing.error ? 'error.main' : 'success.main',
                  }}
                />
              </Stack>
              <Typography noWrap sx={{ fontSize: 11, color: 'text.secondary' }}>
                {n.nodes.length} node{n.nodes.length === 1 ? '' : 's'} · kept #
                {n.indexing.last ?? '—'} · tip #{n.indexing.tip ?? '—'}
              </Typography>
              {n.indexing.error && (
                <Typography sx={{ fontFamily: mono, fontSize: 11, color: 'error.main' }}>
                  {n.indexing.error}
                </Typography>
              )}
            </ListItemButton>
          ))}
          {networks.length === 0 && (
            <Typography color="text.secondary" sx={{ py: 2, fontSize: 13 }}>
              No network is configured.
            </Typography>
          )}
        </List>
      </Box>

      <Box sx={{ px: { xs: 2, md: 4 }, py: { xs: 2.5, md: 3.5 }, minWidth: 0 }}>
        {current ? (
          <Nodes key={nameOf(current)} shell={shell} network={current} onChanged={refresh} />
        ) : (
          <Typography variant="h3" color="text.secondary">
            No network is configured.
          </Typography>
        )}
      </Box>

      <AddNodes
        shell={shell}
        networks={networks}
        open={adding}
        onClose={() => setAdding(false)}
        onAdded={(changed) => {
          setAdding(false);
          shell.notify(`Nodes added to ${changed.map(nameOf).join(', ')}; in use now.`);
          refresh();
        }}
      />
    </Box>
  );
}
