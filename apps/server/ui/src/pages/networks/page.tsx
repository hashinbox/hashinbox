import Add from '@mui/icons-material/Add';
import Hub from '@mui/icons-material/Hub';
import Settings from '@mui/icons-material/Settings';
import Box from '@mui/material/Box';
import IconButton from '@mui/material/IconButton';
import Stack from '@mui/material/Stack';
import Tooltip from '@mui/material/Tooltip';
import { useCallback, useEffect, useState } from 'react';
import { networksList, type ConfiguredNetwork } from '@hashinbox/client';
import { api } from '@/api/client';
import { Empty } from '@/_components/empty';
import type { Shell } from '@/layout';
import { go, useSegments } from '@/route';
import { Nodes } from './_components/nodes';
import { Chain } from './_components/chain';
import { AddNodes } from './_components/add';
import { Split } from '@/_components/split';
import { Side, SideItem } from '@/_components/side';
import { LogsButton } from '@/_components/logs/button';

const BEAT_MS = 4000;
/** How far behind its tip (in time) a network still counts as live. */
const LATE_MS = 30_000;

/** `chain/network`. */
function nameOf(n: Pick<ConfiguredNetwork, 'chain' | 'network'>): string {
  return `${n.chain}/${n.network}`;
}

/** How many of a network's nodes are enabled: none disabled them. */
function enabledOf(n: Pick<ConfiguredNetwork, 'endpoints'>): number {
  return n.endpoints.filter((node) => node.disabled === null).length;
}

/**
 * Where a network's indexing is: green while it is indexed (an index row, a
 * readable node, no failing job) and within its confirmations or 30 s of
 * its tip, amber while further behind or before its first tip, red on a failing job or an unreadable node, grey when the
 * network is not indexed at all.
 */
function syncOf(n: ConfiguredNetwork): { label: string; color: string; live: boolean } {
  const { live: tip } = n.tip;
  const reason = n.index?.job_reason;
  const last = n.index?.indexed;
  if (n.enabled && enabledOf(n) === 0)
    return { label: 'No enabled RPC endpoint', color: 'error.main', live: false };
  if (reason) return { label: 'Retrying after an error', color: 'error.main', live: false };
  if (n.index?.tip == null)
    return n.enabled
      ? { label: 'Waiting for the tip', color: 'warning.main', live: false }
      : { label: 'Not indexed', color: 'text.disabled', live: false };
  // Behind by time, not blocks: a fast chain (Solana: 400 ms slots) is
  // always some blocks behind its live tip while it keeps up.
  const behind = last != null && tip != null ? Math.max(tip - last, 0) : 0;
  const late = behind * n.block_time_ms;
  if (late > Math.max(LATE_MS, n.confirmations * n.block_time_ms))
    return {
      label: `Catching up: ${String(behind)} behind`,
      color: 'warning.main',
      live: false,
    };
  if (last == null) return { label: 'Indexing: no address yet', color: 'success.main', live: true };
  return { label: 'Live at the tip', color: 'success.main', live: true };
}

/** The networks beside the one whose RPC nodes are shown. */
export function Networks({ shell }: { shell: Shell }) {
  const [networks, setNetworks] = useState<ConfiguredNetwork[]>([]);
  const [chains, setChains] = useState<string[]>([]);
  const [adding, setAdding] = useState(false);
  // networks/{chain}/{network}[/settings | /mempool | /blocks/{position}]
  const [, chain, network, page, position] = useSegments();
  const selected = chain && network ? `${chain}/${network}` : null;
  const settings = page === 'settings';

  const refresh = useCallback(() => {
    networksList(api)
      .then((list) => {
        setNetworks(list.networks);
        setChains(list.chains);
      })
      .catch(shell.fail);
  }, [shell.fail]);

  useEffect(() => {
    refresh();
    const timer = window.setInterval(refresh, BEAT_MS);
    return () => window.clearInterval(timer);
  }, [refresh]);

  const current = networks.find((n) => nameOf(n) === selected) ?? null;

  return (
    <>
      <Split
        columns="minmax(220px, 280px) minmax(0, 1fr)"
        open={current !== null}
        onClose={() => go(['networks'])}
        list={
          <Side
            title="Networks"
            actions={
              <Tooltip title="Add nodes: a provider's, or your own">
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
            }
            empty={networks.length === 0 && 'No network is configured.'}
          >
            {networks.map((n) => (
              <SideItem
                key={nameOf(n)}
                name={nameOf(n)}
                dot={syncOf(n)}
                selected={current !== null && nameOf(n) === nameOf(current)}
                onClick={() => go(['networks', n.chain, n.network])}
                detail={`${String(enabledOf(n))}/${String(n.endpoints.length)} node${n.endpoints.length === 1 ? '' : 's'} enabled · indexed #${String(n.index?.indexed ?? '—')} · tip #${String(n.tip.live ?? '—')}`}
                extra={
                  <Tooltip title="Settings">
                    <IconButton
                      size="small"
                      aria-label={`${nameOf(n)} settings`}
                      onClick={(e) => {
                        e.stopPropagation();
                        go(['networks', n.chain, n.network, 'settings']);
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
                }
                problem={n.index?.job_reason}
              />
            ))}
          </Side>
        }
        detail={
          <Box sx={{ px: { xs: 2, md: 4 }, py: { xs: 2.5, md: 3.5 }, minWidth: 0 }}>
            {current && !settings && (
              <Stack direction="row" sx={{ justifyContent: 'flex-end', mb: 2 }}>
                <LogsButton filter={{ network: nameOf(current) }} />
              </Stack>
            )}
            {current && settings ? (
              <Nodes key={nameOf(current)} shell={shell} network={current} onChanged={refresh} />
            ) : current ? (
              <Chain
                key={nameOf(current)}
                shell={shell}
                network={current}
                picked={page === 'blocks' && position ? Number(position) : null}
                onPick={(picked) => {
                  const at = ['networks', current.chain, current.network];
                  go(picked === null ? [...at, 'mempool'] : [...at, 'blocks', String(picked)]);
                }}
              />
            ) : (
              <Empty icon={<Hub />} title="Networks">
                A network is a blockchain this server watches through its RPC nodes. Pick one to see
                its latest blocks, waiting transactions and node settings.
              </Empty>
            )}
          </Box>
        }
      />
      <AddNodes
        shell={shell}
        networks={networks}
        chains={chains}
        open={adding}
        onClose={() => setAdding(false)}
        onAdded={(changed) => {
          setAdding(false);
          shell.notify(
            `Nodes added to ${changed.map((c) => `${c.network.chain}/${c.network.name}`).join(', ')}; in use now.`,
          );
          refresh();
        }}
        onCreated={refresh}
      />
    </>
  );
}
