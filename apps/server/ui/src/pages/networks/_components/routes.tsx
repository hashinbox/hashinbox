import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Chip from '@mui/material/Chip';
import MenuItem from '@mui/material/MenuItem';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useState } from 'react';
import {
  networkRoutesReplace,
  networkRoutesReplaceOperation,
  type ConfiguredNetwork,
  type RpcRoute as Route,
} from '@hashinbox/client';
import { api } from '@/api/client';
import { mono } from '@/theme';
import { fail, notify } from '@/store/app';
import { useCan } from '@/store/session';

/**
 * A network's routed methods: each goes round robin over its own nodes;
 * every other method goes to all of them. Add one, or remove one.
 */
export function Routes({
  network,
  onChanged,
}: {
  network: ConfiguredNetwork;
  onChanged: () => void;
}) {
  const [method, setMethod] = useState('');
  const [picked, setPicked] = useState<number[]>([]);
  const [busy, setBusy] = useState(false);
  const edits = useCan(networkRoutesReplaceOperation, network);
  const label = (id: number) => {
    const index = network.endpoints.findIndex((e) => e.id === id);
    const node = network.endpoints[index];
    return node ? `#${String(index + 1)} ${new URL(node.url).hostname}` : `#${String(id)}`;
  };

  const save = (routes: Route[], done: string) => {
    setBusy(true);
    networkRoutesReplace(api, { chain: network.chain, network: network.network, body: { routes } })
      .then(() => {
        notify(done);
        setMethod('');
        setPicked([]);
        onChanged();
      })
      .catch(fail)
      .finally(() => setBusy(false));
  };

  return (
    <Box component="section" sx={{ mt: 4 }}>
      <Typography variant="h3">Routes</Typography>
      <Typography variant="body2" color="text.secondary" sx={{ mt: 0.5, mb: 2, maxWidth: 720 }}>
        A routed method takes turns over its nodes; every other method fails over as above.
      </Typography>
      <Stack spacing={1}>
        {network.routes.map((route) => (
          <Stack key={route.method} direction="row" spacing={1} sx={{ alignItems: 'center' }}>
            <Typography sx={{ fontFamily: mono, fontSize: 13, minWidth: 180 }}>
              {route.method}
            </Typography>
            <Stack direction="row" spacing={0.5} sx={{ flexGrow: 1, flexWrap: 'wrap' }}>
              {route.endpoint_ids.map((id) => (
                <Chip key={id} size="small" label={label(id)} />
              ))}
            </Stack>
            {edits && (
              <Button
                color="error"
                size="small"
                disabled={busy}
                onClick={() =>
                  save(
                    network.routes.filter((r) => r.method !== route.method),
                    `${route.method} is no longer routed.`,
                  )
                }
              >
                Remove
              </Button>
            )}
          </Stack>
        ))}
        {network.routes.length === 0 && (
          <Typography color="text.secondary" sx={{ fontSize: 13 }}>
            No routed methods.
          </Typography>
        )}
      </Stack>
      {edits && (
        <Stack direction={{ sm: 'row' }} spacing={1} sx={{ mt: 2, alignItems: { sm: 'center' } }}>
          <TextField
            size="small"
            label="Method"
            placeholder="getBlock"
            value={method}
            sx={{ flexGrow: 1 }}
            onChange={(e) => setMethod(e.target.value)}
            slotProps={{ htmlInput: { spellCheck: false, sx: { fontFamily: mono, fontSize: 13 } } }}
          />
          <TextField
            select
            size="small"
            label="Nodes"
            value={picked}
            onChange={(e) => setPicked(e.target.value as unknown as number[])}
            slotProps={{ select: { multiple: true } }}
            sx={{ flexGrow: 1, minWidth: 240 }}
          >
            {network.endpoints.map((e) => (
              <MenuItem key={e.id} value={e.id}>
                {label(e.id)}
              </MenuItem>
            ))}
          </TextField>
          <Button
            variant="contained"
            disableElevation
            loading={busy}
            sx={{ whiteSpace: 'nowrap', flexShrink: 0, height: 40 }}
            disabled={method.trim() === '' || picked.length === 0}
            onClick={() =>
              save(
                [
                  ...network.routes.filter((r) => r.method !== method.trim()),
                  { method: method.trim(), endpoint_ids: picked },
                ],
                `${method.trim()} routed over ${String(picked.length)} node${picked.length === 1 ? '' : 's'}.`,
              )
            }
          >
            Add route
          </Button>
        </Stack>
      )}
    </Box>
  );
}
