import catalog from '@/chains.json';
import Button from '@mui/material/Button';
import Dialog from '@mui/material/Dialog';
import DialogActions from '@mui/material/DialogActions';
import DialogContent from '@mui/material/DialogContent';
import DialogTitle from '@mui/material/DialogTitle';
import MenuItem from '@mui/material/MenuItem';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useState } from 'react';
import { call, client } from '@/api/client';
import type { components } from '@/api/schema';
import type { Shell } from '@/layout';

type Settings = components['schemas']['ChainSettings'];
interface Draft {
  name: string;
  parameters: Record<string, unknown>;
  indexing: 'running' | 'paused';
  saved: boolean;
  nodes: { id: string; endpoint: string }[];
}

export function ChainEditor({
  existing,
  initialChain,
  preset,
  shell,
  close,
}: {
  existing: Settings | null;
  initialChain?: string;
  preset?: { name: string; parameters: Record<string, unknown> };
  shell: Shell;
  close: () => void;
}) {
  const [id, setId] = useState(existing?.id ?? initialChain ?? 'ethereum');
  const [networks, setNetworks] = useState<Draft[]>(() => {
    const held =
      existing?.networks.map((n) => ({
        name: n.name,
        parameters: n.parameters as Record<string, unknown>,
        indexing: n.indexing,
        saved: true,
        nodes: n.nodes.map((node) => ({ id: node.id, endpoint: '' })),
      })) ?? [];
    if (initialChain && !held.some((n) => n.name === preset?.name))
      held.push({
        name: preset?.name ?? '',
        parameters: preset?.parameters ?? {
          asset: initialChain === 'bitcoin' ? 'btc' : initialChain === 'ethereum' ? 'eth' : 'sol',
        },
        indexing: 'running',
        saved: false,
        nodes: [{ id: 'node-1', endpoint: '' }],
      });
    return held;
  });
  const definition = catalog.find((chain) => chain.id === id);
  const [saving, setSaving] = useState(false);
  const edit = (index: number, change: Partial<Draft>) =>
    setNetworks((old) => old.map((n, i) => (i === index ? { ...n, ...change } : n)));
  const save = async () => {
    setSaving(true);
    try {
      for (const n of networks) {
        if (!n.name.trim()) throw new Error('Enter a network name.');
        if (
          !n.saved &&
          id === 'ethereum' &&
          (!Number.isSafeInteger(n.parameters.chain_id) || Number(n.parameters.chain_id) <= 0)
        ) {
          throw new Error('Enter a positive Ethereum chain ID.');
        }
        for (const node of n.nodes) {
          if (!node.endpoint.trim()) {
            if (
              !existing?.networks
                .find((held) => held.name === n.name)
                ?.nodes.some((held) => held.id === node.id)
            )
              throw new Error('Enter a URL for each new RPC node.');
            continue;
          }
          const url = new URL(node.endpoint.trim());
          if (!['http:', 'https:'].includes(url.protocol))
            throw new Error('RPC URLs must use HTTP or HTTPS.');
        }
      }
      await call(
        client.PUT('/v1/chains/{chain}', {
          params: { path: { chain: id } },
          body: {
            revision: existing?.revision ?? null,
            networks: networks.map((n) => ({
              name: n.name.trim(),
              parameters: n.saved ? n.parameters : { ...n.parameters, network: n.name.trim() },
              indexing: n.indexing,
              nodes: n.nodes.map((node) => ({
                id: node.id,
                ...(node.endpoint.trim() ? { endpoint: node.endpoint.trim() } : {}),
              })),
            })),
          },
        }),
      );
      shell.notify('Saved. Connecting to RPC nodes.');
      close();
    } catch (error) {
      shell.fail(error);
    } finally {
      setSaving(false);
    }
  };
  return (
    <Dialog open onClose={saving ? undefined : close} maxWidth="sm" fullWidth>
      <DialogTitle>
        {initialChain ? `Add ${id} RPC node` : existing ? `${id} settings` : 'Add chain'}
      </DialogTitle>
      <DialogContent>
        <Stack spacing={3} sx={{ mt: 1 }}>
          <TextField
            select
            label="Chain"
            value={id}
            disabled={!!existing || networks.length > 0}
            onChange={(e) => setId(e.target.value)}
          >
            {catalog.map((chain) => (
              <MenuItem key={chain.id} value={chain.id}>
                {chain.label}
              </MenuItem>
            ))}
          </TextField>
          {networks.map((n, i) => (
            <Stack key={i} spacing={2} sx={{ borderTop: 1, borderColor: 'divider', pt: 2 }}>
              <Stack direction="row" spacing={2}>
                <TextField
                  select
                  label="Network"
                  value={n.name}
                  disabled={n.saved}
                  onChange={(event) => {
                    const selected = definition?.networks.find(
                      (network) => network.name === event.target.value,
                    );
                    if (selected) edit(i, { name: selected.name, parameters: selected.parameters });
                  }}
                >
                  {definition?.networks.map((network) => (
                    <MenuItem key={network.name} value={network.name}>
                      {network.label}
                    </MenuItem>
                  ))}
                  {n.name && !definition?.networks.some((network) => network.name === n.name) && (
                    <MenuItem value={n.name}>{n.name}</MenuItem>
                  )}
                </TextField>
                <TextField
                  select
                  label="Asset"
                  value={
                    typeof n.parameters.asset === 'string'
                      ? n.parameters.asset
                      : (definition?.assets[0]?.tag ?? '')
                  }
                  disabled={n.saved}
                  onChange={(event) =>
                    edit(i, { parameters: { ...n.parameters, asset: event.target.value } })
                  }
                >
                  {definition?.assets.map((asset) => (
                    <MenuItem
                      key={asset.tag}
                      value={
                        typeof n.parameters.asset === 'string' ? n.parameters.asset : asset.tag
                      }
                    >
                      {asset.symbol}
                    </MenuItem>
                  ))}
                </TextField>
              </Stack>
              <Typography variant="overline">RPC nodes · highest priority first</Typography>
              {n.nodes.map((node, j) => (
                <Stack key={node.id} direction="row" spacing={1} sx={{ alignItems: 'center' }}>
                  <TextField
                    label={node.id}
                    placeholder="https://…"
                    value={node.endpoint}
                    autoComplete="off"
                    helperText={n.saved ? 'Leave blank to keep the saved URL.' : undefined}
                    onChange={(e) =>
                      edit(i, {
                        nodes: n.nodes.map((value, k) =>
                          k === j ? { ...value, endpoint: e.target.value } : value,
                        ),
                      })
                    }
                  />
                  <Button
                    variant="text"
                    disabled={j === 0}
                    onClick={() => {
                      const nodes = [...n.nodes];
                      const previous = nodes[j - 1];
                      if (!previous) return;
                      nodes[j - 1] = node;
                      nodes[j] = previous;
                      edit(i, { nodes });
                    }}
                  >
                    Up
                  </Button>
                  <Button
                    variant="text"
                    onClick={() => edit(i, { nodes: n.nodes.filter((_, k) => j !== k) })}
                  >
                    Remove
                  </Button>
                </Stack>
              ))}
              <Button
                variant="text"
                sx={{ alignSelf: 'flex-start' }}
                onClick={() => {
                  let ordinal = 1;
                  while (n.nodes.some((node) => node.id === `node-${ordinal}`)) ordinal += 1;
                  edit(i, { nodes: [...n.nodes, { id: `node-${ordinal}`, endpoint: '' }] });
                }}
              >
                + RPC node
              </Button>
            </Stack>
          ))}
          <Button
            variant="text"
            sx={{ alignSelf: 'flex-start' }}
            disabled={
              !definition?.networks.some(
                (network) => !networks.some((held) => held.name === network.name),
              )
            }
            onClick={() => {
              const next = definition?.networks.find(
                (network) => !networks.some((held) => held.name === network.name),
              );
              if (next)
                setNetworks((old) => [
                  ...old,
                  {
                    name: next.name,
                    parameters: next.parameters,
                    indexing: 'running',
                    saved: false,
                    nodes: [],
                  },
                ]);
            }}
          >
            + Network
          </Button>
        </Stack>
      </DialogContent>
      <DialogActions>
        <Button variant="text" disabled={saving} onClick={close}>
          Cancel
        </Button>
        <Button disabled={saving || !networks.length} onClick={() => void save()}>
          Save
        </Button>
      </DialogActions>
    </Dialog>
  );
}
