import Alert from '@mui/material/Alert';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Dialog from '@mui/material/Dialog';
import DialogActions from '@mui/material/DialogActions';
import DialogContent from '@mui/material/DialogContent';
import DialogTitle from '@mui/material/DialogTitle';
import MenuItem from '@mui/material/MenuItem';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useCallback, useEffect, useState, type SubmitEvent } from 'react';
import { ApiError, call, client, keyPath, type Schemas, type UserKey } from '@/api/client';
import { Confirm } from '@/_components/confirm';
import { Dot } from '@/_components/dot';
import { When } from '@/_components/when';
import type { Shell } from '@/layout';
import { mono } from '@/theme';
import { Picker } from './picker';
import { pack, short, unpack, type Resources } from './scopes';
import { Section } from './section';

const DAY_MS = 86_400_000;
/** Expiry choices, in days; 0: never. */
const EXPIRIES = [0, 30, 90, 365];

/** Every key of the caller's user, page by page. */
async function allKeys(): Promise<UserKey[]> {
  const keys: UserKey[] = [];
  let cursor: string | undefined;
  do {
    const query = { limit: 500, ...(cursor !== undefined && { cursor }) };
    const page = await call(client.GET('/v1/user/keys', { params: { query } }));
    keys.push(...page.keys);
    cursor = page.next_cursor ?? undefined;
  } while (cursor !== undefined);
  return keys;
}

/** Green active, orange expiring within a week, red revoked or expired. */
function dotOf(key: UserKey, now: number): { color: string; label: string } {
  if (key.revoked_at != null) return { color: 'error.main', label: 'Revoked' };
  if (key.expires_at != null && key.expires_at <= now)
    return { color: 'error.main', label: 'Expired' };
  if (key.expires_at != null && key.expires_at - now < 7 * DAY_MS)
    return { color: 'warning.main', label: 'Expiring' };
  return { color: 'success.main', label: 'Active' };
}

/** The logged-in user's keys: list, create (its secret shown once), rotate, revoke. */
export function Keys({
  shell,
  held,
  resources,
}: {
  shell: Shell;
  held: string[];
  resources: Resources;
}) {
  const [keys, setKeys] = useState<UserKey[] | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [created, setCreated] = useState<Schemas['CreatedKey'] | null>(null);
  const [rotating, setRotating] = useState<UserKey | null>(null);
  const [revoking, setRevoking] = useState<UserKey | null>(null);
  const [now] = useState(Date.now);

  const refresh = useCallback(() => {
    allKeys()
      .then((list) => {
        setKeys(list);
        setProblem(null);
      })
      .catch((error: unknown) => {
        if (error instanceof ApiError && error.status === 403) setProblem(error.message);
        else shell.fail(error);
      });
  }, [shell]);

  useEffect(refresh, [refresh]);

  const rotate = (key: UserKey) => {
    setRotating(null);
    call(client.POST('/v1/user/keys/{id}/rotate', { params: { path: { id: keyPath(key.id) } } }))
      .then(() => {
        shell.notify(`${key.name} rotated: its sessions until now end.`);
        refresh();
      })
      .catch(shell.fail);
  };

  const revoke = (key: UserKey) => {
    setRevoking(null);
    call(client.DELETE('/v1/user/keys/{id}', { params: { path: { id: keyPath(key.id) } } }))
      .then(() => {
        shell.notify(`${key.name} revoked.`);
        refresh();
      })
      .catch(shell.fail);
  };

  return (
    <Section
      title="Keys"
      actions={
        <Button onClick={() => setCreating(true)} disabled={problem !== null}>
          Create key
        </Button>
      }
    >
      {problem && <Alert severity="info">{problem}</Alert>}
      {keys !== null && keys.length === 0 && (
        <Typography color="text.secondary" sx={{ fontSize: 13 }}>
          No key yet. A key lets a server call this API as you, with the scopes you give it.
        </Typography>
      )}
      <Stack spacing={1}>
        {keys?.map((key) => (
          <Box key={key.id} sx={{ border: 1, borderColor: 'divider', borderRadius: 1, p: 1.5 }}>
            <Stack direction="row" spacing={1} sx={{ alignItems: 'center' }}>
              <Typography sx={{ fontWeight: 500, fontSize: 14, flexGrow: 1 }} noWrap>
                {key.name}
              </Typography>
              {key.revoked_at == null && (
                <>
                  <Button onClick={() => setRotating(key)}>Rotate</Button>
                  <Button color="error" onClick={() => setRevoking(key)}>
                    Revoke
                  </Button>
                </>
              )}
              <Dot {...dotOf(key, now)} />
            </Stack>
            <Typography sx={{ fontFamily: mono, fontSize: 12, color: 'text.secondary' }}>
              {key.id}
            </Typography>
            <Typography sx={{ fontFamily: mono, fontSize: 12, mt: 0.5, wordBreak: 'break-all' }}>
              {unpack(key.scopes).map(short).join(' ')}
            </Typography>
            <Typography sx={{ fontSize: 12, color: 'text.secondary', mt: 0.5 }}>
              created <When at={key.created_at} /> ·{' '}
              {key.revoked_at != null ? (
                <>
                  revoked <When at={key.revoked_at} />
                </>
              ) : key.expires_at != null ? (
                <>
                  expires <When at={key.expires_at} />
                </>
              ) : (
                'never expires'
              )}
              {key.last_used_at != null && (
                <>
                  {' '}
                  · last used <When at={key.last_used_at} />
                </>
              )}
            </Typography>
          </Box>
        ))}
      </Stack>

      <Dialog open={creating} onClose={() => setCreating(false)} fullWidth maxWidth="sm">
        <Create
          resources={resources}
          held={held}
          onClose={() => setCreating(false)}
          onCreated={(answer) => {
            setCreating(false);
            setCreated(answer);
            refresh();
          }}
        />
      </Dialog>
      <Secret created={created} shell={shell} onClose={() => setCreated(null)} />
      <Confirm
        open={rotating !== null}
        title={`Rotate ${rotating?.name ?? ''}`}
        phrase={rotating?.name ?? ''}
        action="Rotate"
        onClose={() => setRotating(null)}
        onConfirm={() => {
          if (rotating) rotate(rotating);
        }}
      >
        Every session this key signed until now is refused. The key and its secret keep working.
      </Confirm>
      <Confirm
        open={revoking !== null}
        title={`Revoke ${revoking?.name ?? ''}`}
        phrase={revoking?.name ?? ''}
        action="Revoke"
        onClose={() => setRevoking(null)}
        onConfirm={() => {
          if (revoking) revoke(revoking);
        }}
      >
        The key and every session it signed are refused for good. This cannot be undone.
      </Confirm>
    </Section>
  );
}

/** A new key: its name, scopes from the user's own, and when it expires. */
function Create({
  resources,
  held,
  onClose,
  onCreated,
}: {
  resources: Resources;
  held: string[];
  onClose: () => void;
  onCreated: (created: Schemas['CreatedKey']) => void;
}) {
  const [name, setName] = useState('');
  const [scopes, setScopes] = useState(held);
  const [days, setDays] = useState(90);
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    setBusy(true);
    setProblem(null);
    const body = {
      name: name.trim(),
      scopes: pack(scopes),
      ...(days > 0 && { expires_at: Date.now() + days * DAY_MS }),
    };
    call(client.POST('/v1/user/keys', { body }))
      .then(onCreated)
      .catch((error: unknown) => setProblem(error instanceof Error ? error.message : String(error)))
      .finally(() => setBusy(false));
  };

  return (
    <Box component="form" onSubmit={submit}>
      <DialogTitle variant="h3">Create a key</DialogTitle>
      <DialogContent>
        <Stack spacing={2} sx={{ pt: 1 }}>
          <TextField
            label="Name"
            placeholder="shop backend"
            value={name}
            onChange={(e) => setName(e.target.value)}
            slotProps={{ htmlInput: { maxLength: 128 } }}
            autoFocus
            required
          />
          <TextField
            select
            label="Expires"
            value={days}
            onChange={(e) => setDays(Number(e.target.value))}
          >
            {EXPIRIES.map((d) => (
              <MenuItem key={d} value={d}>
                {d === 0 ? 'Never' : `In ${d} days`}
              </MenuItem>
            ))}
          </TextField>
          <Picker resources={resources} held={held} initial={held} onChange={setScopes} />
          {problem && <Alert severity="error">{problem}</Alert>}
        </Stack>
      </DialogContent>
      <DialogActions>
        <Button color="inherit" onClick={onClose}>
          Cancel
        </Button>
        <Button type="submit" variant="contained" disabled={busy || scopes.length === 0}>
          Create
        </Button>
      </DialogActions>
    </Box>
  );
}

/** A created key's `his/…` secret, this once. */
function Secret({
  created,
  shell,
  onClose,
}: {
  created: Schemas['CreatedKey'] | null;
  shell: Shell;
  onClose: () => void;
}) {
  const secret = created?.secret ?? '';
  return (
    <Dialog open={created !== null} fullWidth maxWidth="sm">
      <DialogTitle variant="h3">{created?.key.name} created</DialogTitle>
      <DialogContent>
        <Alert severity="warning" sx={{ mb: 2 }}>
          Copy the secret now: it is shown this once and never again. Anyone holding it acts as this
          key.
        </Alert>
        <Typography color="text.secondary" sx={{ fontSize: 12 }}>
          Client id{' '}
          <Box component="span" sx={{ fontFamily: mono }}>
            {created?.key.id}
          </Box>
        </Typography>
        <Box
          sx={{
            mt: 1,
            p: 1.5,
            border: 1,
            borderColor: 'divider',
            borderRadius: 1,
            fontFamily: mono,
            fontSize: 13,
            wordBreak: 'break-all',
            userSelect: 'all',
          }}
        >
          {secret}
        </Box>
      </DialogContent>
      <DialogActions>
        <Button
          onClick={() => {
            navigator.clipboard.writeText(secret).then(
              () => shell.notify('Secret copied.'),
              () => shell.notify('Copy failed: select the secret and copy it by hand.', true),
            );
          }}
        >
          Copy secret
        </Button>
        <Button variant="contained" onClick={onClose}>
          I have stored it
        </Button>
      </DialogActions>
    </Dialog>
  );
}
