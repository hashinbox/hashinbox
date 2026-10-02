import catalog from '@/chains.json';
import Alert from '@mui/material/Alert';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import ButtonBase from '@mui/material/ButtonBase';
import Divider from '@mui/material/Divider';
import LinearProgress from '@mui/material/LinearProgress';
import MenuItem from '@mui/material/MenuItem';
import Stack from '@mui/material/Stack';
import List from '@mui/material/List';
import ListItemButton from '@mui/material/ListItemButton';
import ListItemText from '@mui/material/ListItemText';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { mono } from '@/theme';
import { useEffect, useState } from 'react';
import { call, client } from '@/api/client';
import type { components } from '@/api/schema';
import type { Shell } from '@/layout';
import { Events } from './events';
import { ChainEditor } from './editor';
import { NodeEditor } from './node';

type Settings = components['schemas']['ChainSettings'];
type Node = components['schemas']['NodeStatus'];
const domain = (node: Node) => (node.origin ? new URL(node.origin).host : node.id);

export function Chains({ shell }: { shell: Shell }) {
  const [rows, setRows] = useState<Settings[]>([]);
  const [selection, setSelection] = useState(() =>
    window.location.hash.split('/').slice(1).map(decodeURIComponent),
  );
  const [component, setComponent] = useState('');
  const [node, setNode] = useState('');
  const [unavailable, setUnavailable] = useState(false);
  const [nodeEditor, setNodeEditor] = useState<{ chain: Settings; existing?: Node } | null>(null);
  const [editing, setEditing] = useState(false);
  const family = catalog.find((c) => c.id === selection[0]) ??
    catalog[0] ?? { id: 'bitcoin', label: 'Bitcoin', networks: [] };
  const chain = rows.find((c) => c.id === family.id) ?? {
    id: family.id,
    revision: 0,
    networks: [],
  };
  const networkName =
    selection[1] ?? chain.networks[0]?.name ?? family.networks[0]?.name ?? 'mainnet';
  const network = chain.networks.find((n) => n.name === networkName);
  const preset = family.networks.find((n) => n.name === networkName);

  useEffect(() => {
    const navigate = () => {
      setSelection(window.location.hash.split('/').slice(1).map(decodeURIComponent));
      setComponent('');
      setNode('');
    };
    window.addEventListener('hashchange', navigate);
    window.addEventListener('popstate', navigate);
    return () => {
      window.removeEventListener('hashchange', navigate);
      window.removeEventListener('popstate', navigate);
    };
  }, []);

  const select = (id: string, name: string) => {
    setSelection([id, name]);
    setComponent('');
    setNode('');
    window.history.pushState(
      null,
      '',
      `#chains/${encodeURIComponent(id)}/${encodeURIComponent(name)}`,
    );
  };

  useEffect(() => {
    let live = true;
    let timer: number;
    const poll = async () => {
      try {
        const next = await call(client.GET('/v1/chains'));
        if (live) {
          setRows(next);
          setUnavailable(false);
        }
      } catch {
        if (live) setUnavailable(true);
      }
      if (live) timer = window.setTimeout(() => void poll(), 2000);
    };
    void poll();
    return () => {
      live = false;
      window.clearTimeout(timer);
    };
  }, []);

  const control = async (action: 'pause' | 'resume' | 'reindex') => {
    if (
      action === 'reindex' &&
      !window.confirm(
        `Reindex ${family.id}/${networkName}? Indexed facts will be cleared and scanned again. Wallet keys and birthdays are preserved.`,
      )
    )
      return;
    try {
      const { response, error } = await client.PUT('/v1/indexing', {
        body: { network: { chain: family.id, name: networkName }, action },
      });
      if (!response.ok) throw new Error(error?.message ?? 'Index control failed.');
      shell.notify('Request saved.');
    } catch (error) {
      shell.fail(error);
    }
  };

  return (
    <Box sx={{ display: 'flex', minHeight: 'calc(100vh - 64px)', bgcolor: 'background.paper' }}>
      <Box
        component="nav"
        aria-label="Chains and networks"
        sx={{ width: 210, flexShrink: 0, borderRight: 1, borderColor: 'divider', px: 1.5, py: 2 }}
      >
        <Typography variant="overline" sx={{ px: 1, display: 'block', mb: 1 }}>
          Chains
        </Typography>
        <List disablePadding>
          {catalog.map((entry) => {
            const saved = rows.find((c) => c.id === entry.id);
            const networks = [
              ...entry.networks,
              ...(saved?.networks
                .filter((n) => !entry.networks.some((p) => p.name === n.name))
                .map((n) => ({ name: n.name, label: n.name })) ?? []),
            ];
            return (
              <Box key={entry.id} sx={{ mb: 1.5 }}>
                <Typography sx={{ px: 1, py: 0.5, fontSize: 13, fontWeight: 600 }}>
                  {entry.label}
                </Typography>
                {networks.map((n) => {
                  const state = saved?.networks.find((s) => s.name === n.name);
                  return (
                    <ListItemButton
                      key={n.name}
                      selected={family.id === entry.id && networkName === n.name}
                      onClick={() => select(entry.id, n.name)}
                      sx={{
                        minHeight: 30,
                        py: 0.25,
                        pl: 2,
                        pr: 1,
                        borderRadius: 1,
                        borderTop: 0,
                        gap: 1,
                      }}
                    >
                      <ListItemText
                        primary={n.label}
                        slotProps={{ primary: { sx: { fontSize: 12 } } }}
                      />
                      <Box
                        title={
                          state?.error
                            ? 'Needs attention'
                            : state?.nodes.some((n) => n.active)
                              ? 'Connected'
                              : 'Not connected'
                        }
                        sx={{
                          width: 6,
                          height: 6,
                          borderRadius: '50%',
                          bgcolor: state?.error
                            ? 'warning.main'
                            : state?.nodes.some((n) => n.active)
                              ? 'success.main'
                              : 'text.disabled',
                        }}
                      />
                    </ListItemButton>
                  );
                })}
              </Box>
            );
          })}
        </List>
      </Box>
      <Box sx={{ px: 3, py: 2.5, flex: 1, minWidth: 0, maxWidth: 1100 }}>
        <Stack direction="row" sx={{ alignItems: 'center', mb: 2 }}>
          <Typography variant="h3">
            {family.label}{' '}
            <Box component="span" sx={{ color: 'text.secondary', fontWeight: 400 }}>
              / {preset?.label ?? networkName}
            </Box>
          </Typography>
          <Box sx={{ flex: 1 }} />
          {network && (
            <Button variant="text" onClick={() => setEditing(true)}>
              Settings
            </Button>
          )}
          <Button variant="text" onClick={() => setNodeEditor({ chain })}>
            + Add RPC node
          </Button>
        </Stack>
        {unavailable && <Alert severity="warning">Settings unavailable; last sample shown.</Alert>}
        <Stack direction="row" spacing={1} useFlexGap sx={{ flexWrap: 'wrap' }}>
          {network?.nodes.map((rpc, index) => (
            <ButtonBase
              key={rpc.id}
              onClick={() => setNodeEditor({ chain, existing: rpc })}
              sx={{
                display: 'block',
                textAlign: 'left',
                border: 1,
                borderColor: rpc.active ? 'success.main' : 'divider',
                borderRadius: 1.5,
                px: 1.5,
                py: 1.25,
                minWidth: 180,
                '&:hover': { bgcolor: 'action.hover' },
              }}
            >
              <Stack direction="row" spacing={1} sx={{ alignItems: 'center' }}>
                <Box
                  sx={{
                    width: 6,
                    height: 6,
                    borderRadius: '50%',
                    bgcolor: rpc.active ? 'success.main' : 'text.disabled',
                  }}
                />
                <Typography sx={{ fontSize: 13, fontWeight: 600 }}>{domain(rpc)}</Typography>
              </Stack>
              <Typography sx={{ fontSize: 11, color: 'text.secondary', mt: 0.5 }}>
                {index + 1} · {rpc.active ? 'Active' : 'Standby'} · {rpc.attempts.toLocaleString()}{' '}
                requests · {rpc.failures} errors
              </Typography>
            </ButtonBase>
          ))}
          {!network?.nodes.length && (
            <Typography sx={{ fontSize: 12, color: 'text.secondary', py: 1 }}>
              No RPC nodes configured.
            </Typography>
          )}
        </Stack>
        <Divider sx={{ my: 2.5 }} />
        <Stack direction="row" sx={{ alignItems: 'center' }}>
          <Typography sx={{ fontSize: 13, fontWeight: 600 }}>Indexing</Typography>
          <Typography
            sx={{ ml: 1, fontSize: 12, color: 'text.secondary', textTransform: 'capitalize' }}
          >
            {network?.indexing ?? 'Not configured'}
          </Typography>
          <Box sx={{ flex: 1 }} />
          {network && (
            <>
              <Button
                variant="text"
                onClick={() => void control(network.indexing === 'paused' ? 'resume' : 'pause')}
              >
                {network.indexing === 'paused' ? 'Resume' : 'Pause'}
              </Button>
              <Button
                variant="text"
                disabled={!!network.rebuild}
                onClick={() => void control('reindex')}
              >
                Reindex
              </Button>
            </>
          )}
        </Stack>
        {network?.error && (
          <Alert severity="warning" sx={{ mt: 1 }}>
            {network.error}
          </Alert>
        )}
        <Box sx={{ display: 'grid', gridTemplateColumns: 'repeat(3, minmax(0, 1fr))', my: 2 }}>
          {[
            ['Confirmed block', network?.checkpoint ?? '—'],
            ['Pending block', network?.pending_block ?? '—'],
            [
              'History',
              !network
                ? '—'
                : network.history_pending
                  ? `${network.history_pending} watches catching up`
                  : 'No catch-up pending',
            ],
          ].map(([label, value]) => (
            <Box key={label}>
              <Typography variant="overline" sx={{ fontSize: 10 }}>
                {label}
              </Typography>
              <Typography sx={{ fontFamily: mono, fontSize: 14 }}>{value}</Typography>
            </Box>
          ))}
        </Box>
        {network?.rebuild && (
          <Box sx={{ my: 2 }}>
            <Typography sx={{ fontSize: 12 }}>
              Reindex: {network.rebuild.phase} · Target block: {network.rebuild.target ?? 'waiting'}
            </Typography>
            {network.indexing !== 'paused' && <LinearProgress sx={{ mt: 1 }} />}
          </Box>
        )}
        <Divider sx={{ my: 2.5 }} />
        <Typography variant="overline" sx={{ display: 'block', mb: 1.5 }}>
          Indexing & RPC logs
        </Typography>
        <Events
          key={JSON.stringify([family.id, networkName, component, node])}
          chain={family.id}
          network={networkName}
          component={component}
          node={node}
          shell={shell}
          controls={
            <>
              <TextField
                label="Network"
                value={networkName}
                size="small"
                disabled
                sx={{ width: 110, '& .MuiInputBase-root': { fontSize: 12 } }}
              />
              <TextField
                select
                label="Component"
                size="small"
                value={component}
                sx={{ width: 110, '& .MuiInputBase-root': { fontSize: 12 } }}
                onChange={(e) => {
                  setComponent(e.target.value);
                  setNode('');
                }}
              >
                <MenuItem value="">All events</MenuItem>
                <MenuItem value="indexing">Indexing</MenuItem>
                <MenuItem value="rpc">RPC</MenuItem>
                <MenuItem value="wallet">Wallets</MenuItem>
              </TextField>
              <TextField
                select
                label="Node"
                size="small"
                value={node}
                disabled={component === 'indexing'}
                sx={{ width: 160, '& .MuiInputBase-root': { fontSize: 12 } }}
                onChange={(e) => {
                  setNode(e.target.value);
                  setComponent(e.target.value ? 'rpc' : '');
                }}
              >
                <MenuItem value="">All nodes</MenuItem>
                {network?.nodes.map((rpc) => (
                  <MenuItem key={rpc.id} value={rpc.id}>
                    {domain(rpc)} · {rpc.id}
                  </MenuItem>
                ))}
              </TextField>
            </>
          }
        />
      </Box>
      {nodeEditor && (
        <NodeEditor
          chain={nodeEditor.chain}
          networkName={networkName}
          {...(nodeEditor.existing ? { existing: nodeEditor.existing } : {})}
          shell={shell}
          close={() => setNodeEditor(null)}
        />
      )}
      {editing && <ChainEditor existing={chain} shell={shell} close={() => setEditing(false)} />}
    </Box>
  );
}
