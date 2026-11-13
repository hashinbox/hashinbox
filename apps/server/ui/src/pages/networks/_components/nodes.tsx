import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Chip from '@mui/material/Chip';
import List from '@mui/material/List';
import ListItemButton from '@mui/material/ListItemButton';
import Stack from '@mui/material/Stack';
import Table from '@mui/material/Table';
import TableBody from '@mui/material/TableBody';
import TableCell from '@mui/material/TableCell';
import TableRow from '@mui/material/TableRow';
import Typography from '@mui/material/Typography';
import { useState } from 'react';
import { call, client, type ConfiguredNetwork, type Schemas } from '@/api/client';
import { Confirm } from '@/_components/confirm';
import type { Shell } from '@/layout';
import { mono } from '@/theme';
import { AddNode } from './add';
import { Routes } from './routes';

/** A stored node URL, parsed by the browser, and what the server knows of it; `index` is its position. */
interface Node {
  index: number;
  id: number;
  text: string;
  url: URL;
  state: Schemas['RpcNodeState'] | null;
}

const HEADER = 'header.';
const SETTING = 'rpc.';
/** Failures in a row from which a node is tried after every other one. */
const DEMOTED = 3;

/** One line of what the server knows of a node, or that it is not running. */
function summary(state: Node['state']): string {
  if (!state) return 'not running';
  const tip = state.tip === null ? 'tip unknown' : `tip ${state.tip}`;
  return `${tip} · ${state.calls} calls · ${state.failed_calls} failed`;
}

/** A network's nodes in failover order, and the one picked, parsed. */
export function Nodes({
  shell,
  network,
  onChanged,
}: {
  shell: Shell;
  network: ConfiguredNetwork;
  onChanged: () => void;
}) {
  const [picked, setPicked] = useState(0);
  const [adding, setAdding] = useState(false);
  const [removing, setRemoving] = useState<Node | null>(null);
  const nodes = network.endpoints.map(({ id, url, state }, index): Node => ({
    index,
    id,
    text: url,
    url: new URL(url),
    state,
  }));
  const node = nodes[picked] ?? nodes[0] ?? null;
  const name = `${network.chain}/${network.network}`;

  const remove = (target: Node) => {
    setRemoving(null);
    const params = {
      path: { chain: network.chain, network: network.network, id: target.id },
    };
    call(client.DELETE('/v1/networks/{chain}/{network}/endpoints/{id}', { params }))
      .then(() => {
        shell.notify(`${target.url.origin} removed from ${name}.`);
        setPicked(0);
        onChanged();
      })
      .catch(shell.fail);
  };

  return (
    <Box component="article" sx={{ maxWidth: 960 }}>
      <Stack direction="row" sx={{ alignItems: 'center' }}>
        <Typography variant="h2" component="h1" sx={{ fontFamily: mono, flexGrow: 1 }}>
          {name}
        </Typography>
        <Button variant="outlined" onClick={() => setAdding(true)}>
          Add node
        </Button>
      </Stack>
      <Typography variant="body2" color="text.secondary" sx={{ mt: 1, mb: 3, maxWidth: 720 }}>
        Calls go to the healthy node with the highest tip, then to the others in order; a node
        failing 3 times in a row, or rate-limited, goes last. A change is stored and used at once.
        {network.index_job_reason && ` Index job's last run failed: ${network.index_job_reason}`}
      </Typography>
      <Box
        sx={{
          display: 'grid',
          gap: 3,
          gridTemplateColumns: { md: 'minmax(0, 1fr) minmax(0, 1fr)' },
        }}
      >
        <List
          disablePadding
          aria-label="Nodes"
          sx={{ border: 1, borderColor: 'divider', borderRadius: 1 }}
        >
          {nodes.map((n) => (
            <ListItemButton
              key={n.index}
              selected={node?.index === n.index}
              onClick={() => setPicked(n.index)}
              sx={{ display: 'block' }}
            >
              <Stack direction="row" spacing={1} sx={{ alignItems: 'center' }}>
                <Typography sx={{ fontFamily: mono, fontSize: 11, color: 'text.secondary' }}>
                  #{n.index + 1}
                </Typography>
                <Typography noWrap sx={{ fontFamily: mono, fontSize: 13, flexGrow: 1 }}>
                  {n.url.hostname}
                </Typography>
                {[...n.url.searchParams.keys()].some((k) => k.startsWith(HEADER)) && (
                  <Chip size="small" label="header key" />
                )}
                {n.state && n.state.failures >= DEMOTED && (
                  <Chip size="small" color="warning" label="demoted" />
                )}
              </Stack>
              <Typography noWrap sx={{ fontFamily: mono, fontSize: 11.5, color: 'text.secondary' }}>
                {n.text}
              </Typography>
              <Typography noWrap sx={{ fontFamily: mono, fontSize: 11.5, color: 'text.secondary' }}>
                {summary(n.state)}
              </Typography>
            </ListItemButton>
          ))}
          {nodes.length === 0 && (
            <Typography color="text.secondary" sx={{ p: 2, fontSize: 13 }}>
              No stored nodes.
            </Typography>
          )}
        </List>
        {node && (
          <Detail node={node} onRemove={() => setRemoving(node)} last={nodes.length === 1} />
        )}
      </Box>
      <Routes shell={shell} network={network} onChanged={onChanged} />
      <AddNode
        shell={shell}
        network={network}
        open={adding}
        onClose={() => setAdding(false)}
        onAdded={() => {
          setAdding(false);
          shell.notify(`Node added to ${name}; in use now.`);
          onChanged();
        }}
      />
      <Confirm
        open={removing !== null}
        title={`Remove ${removing?.url.hostname ?? ''}`}
        phrase={removing?.url.hostname ?? ''}
        action="Remove"
        onClose={() => setRemoving(null)}
        onConfirm={() => {
          if (removing) remove(removing);
        }}
      >
        Calls of this network stop going to this node at once, and its routes go with it.
      </Confirm>
    </Box>
  );
}

/** One node's URL, parsed: an unwritten port or `rpc.` setting is the default. */
function Detail({ node, onRemove, last }: { node: Node; onRemove: () => void; last: boolean }) {
  const row = (label: string, value: string) => (
    <TableRow>
      <TableCell sx={{ color: 'text.secondary', width: 150 }}>{label}</TableCell>
      <TableCell sx={{ fontFamily: mono, fontSize: 12.5, overflowWrap: 'anywhere' }}>
        {value}
      </TableCell>
    </TableRow>
  );
  const { url, state } = node;
  const params = [...url.searchParams];
  const query = params.filter(([k]) => !k.startsWith(HEADER) && !k.startsWith(SETTING));
  const headers = params.filter(([k]) => k.startsWith(HEADER));
  const setting = (name: string, unit: string) => {
    const value = url.searchParams.get(SETTING + name);
    return value === null ? 'default' : `${value}${unit}`;
  };
  return (
    <Box>
      <Table size="small">
        <TableBody>
          {row('URL', node.text)}
          {row('Scheme', url.protocol.replace(/:$/, ''))}
          {row('Host', url.hostname)}
          {row('Port', url.port || 'default')}
          {row('Path', url.pathname)}
          {row(
            'User and password',
            url.username || url.password
              ? `${decodeURIComponent(url.username)}:${decodeURIComponent(url.password)} (HTTP Basic)`
              : 'none',
          )}
          {row('Query', query.length ? query.map(([k, v]) => `${k}=${v}`).join(' ') : 'none')}
          {row(
            'Headers',
            headers.length
              ? headers.map(([k, v]) => `${k.slice(HEADER.length)}: ${v}`).join(', ')
              : 'none',
          )}
          {row('Timeout', setting('timeout_ms', ' ms'))}
          {row('Request limit', setting('max_request_bytes', ' bytes'))}
          {row('Response limit', setting('max_response_bytes', ' bytes'))}
          {row('Max batch', setting('max_batch', ' calls'))}
          {row('Tip', state?.tip?.toString() ?? 'unknown')}
          {row('Failures in a row', state ? state.failures.toString() : 'not running')}
          {row('Calls', state ? `${state.calls} (${state.failed_calls} failed)` : 'not running')}
          {row('Last failure', state?.last_failure ?? 'none')}
        </TableBody>
      </Table>
      <Button color="error" sx={{ mt: 1.5 }} disabled={last} onClick={onRemove}>
        {last ? 'The last node stays' : 'Remove node'}
      </Button>
    </Box>
  );
}
