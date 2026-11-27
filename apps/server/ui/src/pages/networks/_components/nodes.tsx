import Alert from '@mui/material/Alert';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import FormControlLabel from '@mui/material/FormControlLabel';
import Switch from '@mui/material/Switch';
import Chip from '@mui/material/Chip';
import List from '@mui/material/List';
import ListItemButton from '@mui/material/ListItemButton';
import Stack from '@mui/material/Stack';
import Table from '@mui/material/Table';
import TableBody from '@mui/material/TableBody';
import TableCell from '@mui/material/TableCell';
import TableRow from '@mui/material/TableRow';
import Typography from '@mui/material/Typography';
import { useEffect, useState } from 'react';
import {
  networkEndpointDelete,
  networkEndpointDeleteOperation,
  networkEndpointsCreateOperation,
  networkEndpointStateUpdateOperation,
  networkEndpointFailuresList,
  networkEndpointStateUpdate,
  type ConfiguredNetwork,
  type RpcNode,
  type RpcNodeFailure,
} from '@hashinbox/client';
import { api } from '@/api/client';
import { Confirm } from '@/_components/confirm';
import { When } from '@/_components/when';
import { mono } from '@/theme';
import { AddNode } from './add';
import { Routes } from './routes';
import { fail, notify } from '@/store/app';
import { useCan } from '@/store/session';

/** A stored node as the server lists it, its URL parsed by the browser; `index` is its position. */
interface Node {
  index: number;
  text: string;
  url: URL;
  enabled: boolean;
  stored: RpcNode;
}

const HEADER = 'header.';
const SETTING = 'rpc.';

/** One line of a node's calls. */
function summary(node: RpcNode): string {
  return `${node.stats.calls} calls · ${node.stats.failed_calls} failed`;
}

/** A network's nodes in failover order, and the one picked, parsed. */
export function Nodes({
  network,
  onChanged,
}: {
  network: ConfiguredNetwork;
  onChanged: () => void;
}) {
  const [picked, setPicked] = useState(0);
  const [adding, setAdding] = useState(false);
  const [removing, setRemoving] = useState<Node | null>(null);
  const nodes = network.endpoints.map((stored, index): Node => ({
    index,
    text: stored.url,
    url: new URL(stored.url),
    enabled: stored.disabled === null,
    stored,
  }));
  const node = nodes[picked] ?? nodes[0] ?? null;
  const name = `${network.chain}/${network.network}`;
  const adds = useCan(networkEndpointsCreateOperation, network);

  const remove = (target: Node) => {
    setRemoving(null);
    const params = { chain: network.chain, network: network.network, id: target.stored.id };
    networkEndpointDelete(api, params)
      .then(() => {
        notify(`${target.url.origin} removed from ${name}.`);
        setPicked(0);
        onChanged();
      })
      .catch(fail);
  };

  const toggle = (target: Node) => {
    const params = { chain: network.chain, network: network.network, id: target.stored.id };
    const body = { state: target.enabled ? ('disabled' as const) : ('enabled' as const) };
    networkEndpointStateUpdate(api, { ...params, body })
      .then(() => {
        notify(`${target.url.origin} ${target.enabled ? 'disabled' : 'enabled'}.`);
        onChanged();
      })
      .catch(fail);
  };

  return (
    <Box component="article" sx={{ maxWidth: 960 }}>
      <Stack direction="row" sx={{ alignItems: 'center' }}>
        <Typography variant="h2" component="h1" sx={{ fontFamily: mono, flexGrow: 1 }}>
          {name}
        </Typography>
        {adds && (
          <Button variant="outlined" onClick={() => setAdding(true)}>
            Add node
          </Button>
        )}
      </Stack>
      <Typography variant="body2" color="text.secondary" sx={{ mt: 1, mb: 3, maxWidth: 720 }}>
        Calls go to the healthy node with the highest tip, then to the others in order; a node
        failing 3 times in a row, or rate-limited, goes last. A change is stored and used at once.
        {network.index?.job_reason && ` Index job's last run failed: ${network.index.job_reason}`}
      </Typography>
      {nodes.every((n) => !n.enabled) && (
        <Alert severity="error" sx={{ mb: 3 }}>
          No enabled RPC endpoint: no call goes out, indexing idles and payments wait. Enable a
          node.
        </Alert>
      )}
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
                {!n.enabled && <Chip size="small" color="error" label="Disabled" />}
              </Stack>
              {n.stored.disabled && (
                <Typography noWrap sx={{ fontSize: 11.5, color: 'error.main' }}>
                  {n.stored.disabled.message}
                </Typography>
              )}
              <Typography noWrap sx={{ fontFamily: mono, fontSize: 11.5, color: 'text.secondary' }}>
                {n.text}
              </Typography>
              <Typography noWrap sx={{ fontFamily: mono, fontSize: 11.5, color: 'text.secondary' }}>
                {summary(n.stored)}
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
          <Detail
            key={node.stored.id}
            node={node}
            network={network}
            onRemove={() => setRemoving(node)}
            onToggle={() => toggle(node)}
            last={nodes.length === 1}
          />
        )}
      </Box>
      <Routes network={network} onChanged={onChanged} />
      <AddNode
        network={network}
        open={adding}
        onClose={() => setAdding(false)}
        onAdded={() => {
          setAdding(false);
          notify(`Node added to ${name}; in use now.`);
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

/** One node's URL, parsed: an unwritten port or `rpc.` setting is the default; its state, and its recent failures. */
function Detail({
  node,
  network,
  onRemove,
  onToggle,
  last,
}: {
  node: Node;
  network: ConfiguredNetwork;
  onRemove: () => void;
  onToggle: () => void;
  last: boolean;
}) {
  const own = { chain: network.chain, network: network.network, id: node.stored.id };
  const changes = useCan(networkEndpointStateUpdateOperation, own);
  const removes = useCan(networkEndpointDeleteOperation, own);
  const row = (label: string, value: string) => (
    <TableRow>
      <TableCell sx={{ color: 'text.secondary', width: 150 }}>{label}</TableCell>
      <TableCell sx={{ fontFamily: mono, fontSize: 12.5, overflowWrap: 'anywhere' }}>
        {value}
      </TableCell>
    </TableRow>
  );
  const { url, stored } = node;
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
          {row('Calls', `${stored.stats.calls} (${stored.stats.failed_calls} failed)`)}
          {row(
            'State',
            stored.disabled
              ? `disabled (${stored.disabled.kind}): ${stored.disabled.message}`
              : 'enabled',
          )}
        </TableBody>
      </Table>
      <Stack direction="row" spacing={1} sx={{ mt: 1.5, alignItems: 'center' }}>
        {changes && (
          <FormControlLabel
            control={<Switch checked={node.enabled} onChange={onToggle} />}
            label={node.enabled ? 'Enabled' : 'Disabled'}
          />
        )}
        <Box sx={{ flexGrow: 1 }} />
        {removes && (
          <Button color="error" disabled={last} onClick={onRemove}>
            {last ? 'The last node stays' : 'Remove node'}
          </Button>
        )}
      </Stack>
      {/* Read again whenever the node is disabled or enabled. */}
      <Failures key={stored.disabled?.id ?? 0} network={network} id={stored.id} />
    </Box>
  );
}

/** What last disabled or enabled a node, and why, newest first. */
function Failures({ network, id }: { network: ConfiguredNetwork; id: number }) {
  const [failures, setFailures] = useState<RpcNodeFailure[] | null>(null);
  useEffect(() => {
    let live = true;
    networkEndpointFailuresList(api, {
      chain: network.chain,
      network: network.network,
      id,
      limit: 20,
    })
      .then((page) => {
        if (live) setFailures(page.failures);
      })
      .catch(() => {
        if (live) setFailures([]);
      });
    return () => {
      live = false;
    };
  }, [network.chain, network.network, id]);
  return (
    <Box sx={{ mt: 2 }}>
      <Typography variant="subtitle2">Disables and enables</Typography>
      {failures?.length === 0 && (
        <Typography color="text.secondary" sx={{ fontSize: 13 }}>
          None yet.
        </Typography>
      )}
      <Table size="small">
        <TableBody>
          {failures?.map((f) => (
            <TableRow key={f.id}>
              <TableCell sx={{ fontSize: 12 }}>
                <When at={f.at} />
              </TableCell>
              <TableCell sx={{ fontFamily: mono, fontSize: 12 }}>{f.kind}</TableCell>
              <TableCell sx={{ fontSize: 12, overflowWrap: 'anywhere' }}>{f.message}</TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </Box>
  );
}
