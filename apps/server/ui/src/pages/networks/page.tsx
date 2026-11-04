import Add from '@mui/icons-material/Add';
import Hub from '@mui/icons-material/Hub';
import Settings from '@mui/icons-material/Settings';
import Box from '@mui/material/Box';
import IconButton from '@mui/material/IconButton';
import List from '@mui/material/List';
import ListItemButton from '@mui/material/ListItemButton';
import Stack from '@mui/material/Stack';
import Tooltip from '@mui/material/Tooltip';
import Typography from '@mui/material/Typography';
import { useCallback, useEffect, useState } from 'react';
import { call, client, type ConfiguredNetwork } from '@/api/client';
import { Empty } from '@/_components/empty';
import type { Shell } from '@/layout';
import { go, useSegments } from '@/route';
import { mono } from '@/theme';
import { Nodes } from './_components/nodes';
import { Chain } from './_components/chain';
import { AddNodes } from './_components/add';

const BEAT_MS = 4000;

/** `chain/network`. */
function nameOf(n: Pick<ConfiguredNetwork, 'network'>): string {
  return `${n.network.chain}/${n.network.name}`;
}

/** Where a network's indexing is: live at the tip, catching up, idle (nothing to index), or failing. */
function syncOf(n: ConfiguredNetwork): { label: string; color: string; live: boolean } {
  const { index_error: error, indexed: last, tip_live: tip } = n;
  if (error) return { label: 'Retrying after an error', color: 'error.main', live: false };
  if (last == null || tip == null)
    return { label: 'Idle: no address to index', color: 'text.disabled', live: false };
  if (tip - last > 1)
    return {
      label: `Catching up: ${String(tip - last)} behind`,
      color: 'warning.main',
      live: false,
    };
  return { label: 'Live at the tip', color: 'success.main', live: true };
}

/** The networks beside the one whose RPC nodes are shown. */
export function Networks({ shell }: { shell: Shell }) {
  const [networks, setNetworks] = useState<ConfiguredNetwork[]>([]);
  const [adding, setAdding] = useState(false);
  // networks/{chain}/{network}[/settings | /mempool | /blocks/{position}]
  const [, chain, network, page, position] = useSegments();
  const selected = chain && network ? `${chain}/${network}` : null;
  const settings = page === 'settings';

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

  const current = networks.find((n) => nameOf(n) === selected) ?? null;

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
            <IconButton
              size="small"
              aria-label="Add RPC nodes"
              onClick={(e) => {
                e.currentTarget.blur();
                setAdding(true);
              }}
            >
              <Add fontSize="small" />
            </IconButton>
          </Tooltip>
        </Stack>
        <List disablePadding aria-label="Networks">
          {networks.map((n) => (
            <ListItemButton
              key={nameOf(n)}
              selected={current !== null && nameOf(n) === nameOf(current)}
              onClick={() => go(['networks', n.network.chain, n.network.name])}
              sx={{ py: 1.25, px: 1, display: 'block' }}
            >
              <Stack direction="row" spacing={1} sx={{ alignItems: 'center' }}>
                <Typography noWrap sx={{ fontFamily: mono, fontSize: 13, flexGrow: 1 }}>
                  {nameOf(n)}
                </Typography>
                <Box sx={{ width: 20, mr: -0.5, display: 'flex', justifyContent: 'center' }}>
                  <Dot sync={syncOf(n)} />
                </Box>
              </Stack>
              <Stack direction="row" spacing={1} sx={{ alignItems: 'center' }}>
                <Typography noWrap sx={{ fontSize: 11, color: 'text.secondary', flexGrow: 1 }}>
                  {n.endpoints.length} node{n.endpoints.length === 1 ? '' : 's'} · indexed #
                  {n.indexed ?? '—'} · tip #{n.tip_live ?? '—'}
                </Typography>
                <Tooltip title="Settings">
                  <IconButton
                    size="small"
                    aria-label={`${nameOf(n)} settings`}
                    onClick={(e) => {
                      e.stopPropagation();
                      go(['networks', n.network.chain, n.network.name, 'settings']);
                    }}
                    sx={{
                      p: 0.25,
                      mr: -0.5,
                      color:
                        settings && current !== null && nameOf(n) === nameOf(current)
                          ? 'text.primary'
                          : 'text.secondary',
                    }}
                  >
                    <Settings sx={{ fontSize: 16 }} />
                  </IconButton>
                </Tooltip>
              </Stack>
              {n.index_error && (
                <Typography sx={{ fontFamily: mono, fontSize: 11, color: 'error.main' }}>
                  {n.index_error}
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
        {current && settings ? (
          <Nodes key={nameOf(current)} shell={shell} network={current} onChanged={refresh} />
        ) : current ? (
          <Chain
            key={nameOf(current)}
            shell={shell}
            network={current}
            picked={page === 'blocks' && position ? Number(position) : null}
            onPick={(picked) => {
              const at = ['networks', current.network.chain, current.network.name];
              go(picked === null ? [...at, 'mempool'] : [...at, 'blocks', String(picked)]);
            }}
          />
        ) : (
          <Empty icon={<Hub />} title="Networks">
            A network is a blockchain this server watches through its RPC nodes. Pick one to see its
            latest blocks, waiting transactions and node settings.
          </Empty>
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

/** A network's sync state as a dot; live, it pulses. Named on hover. */
function Dot({ sync }: { sync: { label: string; color: string; live: boolean } }) {
  return (
    <Box
      component="span"
      role="img"
      aria-label={sync.label}
      title={sync.label}
      sx={{
        position: 'relative',
        width: 8,
        height: 8,
        borderRadius: '50%',
        flex: 'none',
        bgcolor: sync.color,
        '@keyframes pulse': {
          from: { transform: 'scale(1)', opacity: 0.6 },
          to: { transform: 'scale(2.6)', opacity: 0 },
        },
        '&::after': sync.live
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
  );
}
