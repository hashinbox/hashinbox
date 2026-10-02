import Button from '@mui/material/Button';
import Dialog from '@mui/material/Dialog';
import DialogActions from '@mui/material/DialogActions';
import DialogContent from '@mui/material/DialogContent';
import DialogTitle from '@mui/material/DialogTitle';
import catalog from '@/chains.json';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useState } from 'react';
import { call, client } from '@/api/client';
import type { components } from '@/api/schema';
import type { Shell } from '@/layout';

type Settings = components['schemas']['ChainSettings'];

export function NodeEditor({
  chain,
  networkName,
  shell,
  close,
  existing,
}: {
  chain: Settings;
  networkName: string;
  shell: Shell;
  close: () => void;
  existing?: components['schemas']['NodeStatus'];
}) {
  const network = networkName;
  const [name, setName] = useState(existing?.id ?? '');
  const [url, setUrl] = useState(existing?.url ?? '');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const save = async () => {
    setSaving(true);
    setError('');
    try {
      const preset = catalog
        .find((c) => c.id === chain.id)
        ?.networks.find((n) => n.name === network);
      const chosen =
        chain.networks.find((n) => n.name === network) ??
        (preset
          ? {
              name: network,
              parameters: preset.parameters,
              indexing: 'running' as const,
              nodes: [],
            }
          : undefined);
      if (!chosen) throw new Error('Network configuration unavailable.');
      const networks = chain.networks.some((n) => n.name === network)
        ? chain.networks
        : [...chain.networks, chosen];
      const id = name.trim();
      if (!id) throw new Error('Enter a node name.');
      if (!existing && chosen.nodes.some((node) => node.id === id))
        throw new Error('That node name is already in use.');
      const endpoint = url.trim();
      if (endpoint && endpoint !== existing?.url) {
        const parsed = new URL(endpoint);
        if (!['http:', 'https:'].includes(parsed.protocol))
          throw new Error('Use an HTTP or HTTPS URL.');
      } else if (!endpoint) throw new Error('Enter an RPC URL.');
      await call(
        client.PUT('/v1/chains/{chain}', {
          params: { path: { chain: chain.id } },
          body: {
            revision: chain.networks.length ? chain.revision : null,
            networks: networks.map((n) => ({
              name: n.name,
              parameters: n.parameters,
              indexing: n.indexing,
              nodes: [
                ...n.nodes.map((node) => ({
                  id: node.id,
                  ...(n.name === network &&
                  node.id === existing?.id &&
                  endpoint &&
                  endpoint !== existing.url
                    ? { endpoint }
                    : {}),
                })),
                ...(n.name === network && !existing ? [{ id, endpoint }] : []),
              ],
            })),
          },
        }),
      );
      shell.notify(existing ? 'RPC node updated. Connecting.' : 'RPC node added. Connecting.');
      close();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setSaving(false);
    }
  };
  return (
    <Dialog open onClose={saving ? undefined : close} maxWidth="xs" fullWidth>
      <DialogTitle>{existing ? (existing.origin ?? existing.id) : 'Add RPC node'}</DialogTitle>
      <DialogContent>
        <Stack
          component="form"
          id="add-rpc-node"
          spacing={2}
          sx={{ pt: 1 }}
          onSubmit={(event) => {
            event.preventDefault();
            void save();
          }}
        >
          <TextField
            label="Node name"
            placeholder="primary-rpc"
            value={name}
            required
            disabled={saving || !!existing}
            autoFocus={!existing}
            onChange={(event) => setName(event.target.value)}
          />
          <TextField
            label="RPC URL"
            placeholder="https://…"
            type="url"
            autoFocus={!!existing}
            value={url}
            required
            helperText={existing ? 'Edit the URL to update this node.' : undefined}
            disabled={saving}
            autoComplete="off"
            onChange={(event) => setUrl(event.target.value)}
          />
          {error && (
            <Typography role="alert" sx={{ fontSize: 12, color: 'error.main' }}>
              {error}
            </Typography>
          )}
        </Stack>
      </DialogContent>
      <DialogActions>
        <Button variant="text" disabled={saving} onClick={close}>
          Cancel
        </Button>
        <Button
          type="submit"
          form="add-rpc-node"
          disabled={saving || !network || !name.trim() || !url.trim()}
        >
          {existing ? 'Save' : 'Add node'}
        </Button>
      </DialogActions>
    </Dialog>
  );
}
