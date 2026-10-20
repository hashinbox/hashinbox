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

type Node = Schemas['RpcNode'];

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
  const node = network.nodes[picked] ?? network.nodes[0] ?? null;
  const name = `${network.network.chain}/${network.network.name}`;

  const remove = (target: Node) => {
    setRemoving(null);
    const params = {
      path: { chain: network.network.chain, network: network.network.name, index: target.index },
    };
    call(client.DELETE('/v1/networks/{chain}/{network}/rpc/{index}', { params }))
      .then(() => {
        shell.notify(`${target.origin} removed from ${name}.`);
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
        Calls go to the first node and fail over in order. A change is stored and used at once.
        {network.indexing.error && ` Last indexing error: ${network.indexing.error}`}
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
          {network.nodes.map((n) => (
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
                  {n.host}
                </Typography>
                {n.parameters.some((p) => p.header) && <Chip size="small" label="header key" />}
              </Stack>
              <Typography noWrap sx={{ fontFamily: mono, fontSize: 11.5, color: 'text.secondary' }}>
                {n.url}
              </Typography>
            </ListItemButton>
          ))}
          {network.nodes.length === 0 && (
            <Typography color="text.secondary" sx={{ p: 2, fontSize: 13 }}>
              No stored nodes.
            </Typography>
          )}
        </List>
        {node && (
          <Detail
            node={node}
            onRemove={() => setRemoving(node)}
            last={network.nodes.length === 1}
          />
        )}
      </Box>
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
        title={`Remove ${removing?.host ?? ''}`}
        phrase={removing?.host ?? ''}
        action="Remove"
        onClose={() => setRemoving(null)}
        onConfirm={() => {
          if (removing) remove(removing);
        }}
      >
        Calls of this network stop going to this node at once.
      </Confirm>
    </Box>
  );
}

/** One node's URL, parsed. */
function Detail({ node, onRemove, last }: { node: Node; onRemove: () => void; last: boolean }) {
  const row = (label: string, value: string) => (
    <TableRow>
      <TableCell sx={{ color: 'text.secondary', width: 150 }}>{label}</TableCell>
      <TableCell sx={{ fontFamily: mono, fontSize: 12.5, overflowWrap: 'anywhere' }}>
        {value}
      </TableCell>
    </TableRow>
  );
  const query = node.parameters.filter((p) => !p.header);
  const headers = node.parameters.filter((p) => p.header);
  return (
    <Box>
      <Table size="small">
        <TableBody>
          {row('URL', node.url)}
          {row('Scheme', node.scheme)}
          {row('Host', node.host)}
          {row('Port', node.port == null ? '—' : String(node.port))}
          {row('Path', node.path || '/')}
          {row(
            'User and password',
            node.username || node.password
              ? `${node.username}:${node.password ?? ''} (HTTP Basic)`
              : 'none',
          )}
          {row('Query', query.length ? query.map((p) => `${p.name}=${p.value}`).join(' ') : 'none')}
          {row(
            'Headers',
            headers.length ? headers.map((p) => `${p.name}: ${p.value}`).join(', ') : 'none',
          )}
          {row('Timeout', `${node.timeout_ms} ms`)}
          {row('Request limit', `${node.max_request_bytes} bytes`)}
          {row('Response limit', `${node.max_response_bytes} bytes`)}
        </TableBody>
      </Table>
      <Button color="error" sx={{ mt: 1.5 }} disabled={last} onClick={onRemove}>
        {last ? 'The last node stays' : 'Remove node'}
      </Button>
    </Box>
  );
}
