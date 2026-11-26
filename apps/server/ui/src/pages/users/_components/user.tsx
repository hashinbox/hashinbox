import Alert from '@mui/material/Alert';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Chip from '@mui/material/Chip';
import Dialog from '@mui/material/Dialog';
import DialogActions from '@mui/material/DialogActions';
import DialogContent from '@mui/material/DialogContent';
import DialogTitle from '@mui/material/DialogTitle';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useState, type SubmitEvent } from 'react';
import { call, client, type Schemas, type User, type UserRead } from '@/api/client';
import { Confirm } from '@/_components/confirm';
import { When } from '@/_components/when';
import type { Shell } from '@/layout';
import { mono } from '@/theme';
import { Keys } from './keys';
import { Picker, ScopeList } from './picker';
import { ROOT, allows, pack, unpack, type Resources } from './scopes';
import { Recovery } from './recovery';
import { Row, Section } from './section';
import { Sessions } from './sessions';
import { TwoFactor } from './two-factor';

/** What the logged-in user's own page reports back to the shell. */
export interface Own {
  onLogout: (everywhere: boolean) => void;
  /** The server ended every session already (password changed, 2FA off). */
  onSignedOut: () => void;
  /** A fresh session for this tab (the password changed). */
  onSession: (token: string) => void;
}

/**
 * One user: its profile, and with `own` (the logged-in user's page) its
 * password, keys, two-factor, session and recovery too.
 */
export function UserDetail({
  shell,
  me,
  user,
  resources,
  onChanged,
  own,
}: {
  shell: Shell;
  me: UserRead;
  user: User;
  resources: Resources;
  onChanged: () => void;
  own?: Own;
}) {
  const held = unpack(me.user.scopes);
  const manages = user.id !== me.user.id && allows(held, `${ROOT}.users.$write`);
  const disabled = user.disabled_at != null;
  const [password, setPassword] = useState(false);
  const [editing, setEditing] = useState(false);
  const [disabling, setDisabling] = useState(false);

  const update = (body: Schemas['UpdateUserRequest'], done: string) => {
    call(client.PATCH('/v1/users/{id}', { params: { path: { id: user.id } }, body }))
      .then(() => {
        shell.notify(done);
        onChanged();
      })
      .catch(shell.fail);
  };

  return (
    <Box component="article" sx={{ maxWidth: 960 }}>
      <Stack direction="row" spacing={1} sx={{ alignItems: 'center' }}>
        <Typography variant="h2" component="h1" sx={{ flexGrow: 1, minWidth: 0 }} noWrap>
          {user.name}
        </Typography>
        {disabled && <Chip size="small" color="error" label="Disabled" />}
        {!user.login && <Chip size="small" label="No dashboard login" />}
      </Stack>
      <Typography color="text.secondary" sx={{ fontFamily: mono, fontSize: 13, mt: 0.5 }}>
        {user.email}
      </Typography>

      <Section
        title="Profile"
        actions={
          <>
            {own && <Button onClick={() => setPassword(true)}>Change password</Button>}
            {manages && <Button onClick={() => setEditing(true)}>Change scopes</Button>}
            {manages &&
              (disabled ? (
                <Button onClick={() => update({ disabled: false }, `${user.name} enabled.`)}>
                  Enable
                </Button>
              ) : (
                <Button color="error" onClick={() => setDisabling(true)}>
                  Disable
                </Button>
              ))}
          </>
        }
      >
        <Row label="Name">{user.name}</Row>
        <Row label="Email">{user.email}</Row>
        <Row label="Created">
          <When at={user.created_at} />
        </Row>
        {user.disabled_at != null && (
          <Row label="Disabled">
            <When at={user.disabled_at} />
          </Row>
        )}
        <Row label="Scopes">
          <ScopeList scopes={unpack(user.scopes)} />
        </Row>
      </Section>

      {own && (
        <>
          <Keys shell={shell} held={held} resources={resources} />
          <TwoFactor
            shell={shell}
            state={me.two_factor}
            onChanged={onChanged}
            onSignedOut={own.onSignedOut}
          />
          <Sessions shell={shell} onLogout={own.onLogout} />
          {allows(held, `${ROOT}.recovery.$write`) && <Recovery shell={shell} />}
          <Password
            open={password}
            onClose={() => setPassword(false)}
            onChanged={(token) => {
              setPassword(false);
              own.onSession(token);
              shell.notify('Password changed: your other sessions are signed out.');
            }}
          />
        </>
      )}

      <Dialog open={editing} onClose={() => setEditing(false)} fullWidth maxWidth="sm">
        <ScopesForm
          resources={resources}
          held={held}
          initial={unpack(user.scopes)}
          onClose={() => setEditing(false)}
          onSave={(scopes) => {
            setEditing(false);
            update({ scopes: pack(scopes) }, `${user.name}'s scopes changed.`);
          }}
        />
      </Dialog>
      <Confirm
        open={disabling}
        title={`Disable ${user.name}`}
        phrase={user.email}
        action="Disable"
        onClose={() => setDisabling(false)}
        onConfirm={() => {
          setDisabling(false);
          update({ disabled: true }, `${user.name} disabled.`);
        }}
      >
        The user, its keys and every session it issued are refused from the next request on.
      </Confirm>
    </Box>
  );
}

/** Another user's scopes, picked from the caller's. */
function ScopesForm({
  resources,
  held,
  initial,
  onClose,
  onSave,
}: {
  resources: Resources;
  held: string[];
  initial: string[];
  onClose: () => void;
  onSave: (scopes: string[]) => void;
}) {
  const [scopes, setScopes] = useState(initial);
  return (
    <>
      <DialogTitle variant="h3">Change scopes</DialogTitle>
      <DialogContent>
        <Typography color="text.secondary" sx={{ fontSize: 13, mb: 2 }}>
          Narrowing a user narrows its keys and sessions at once.
        </Typography>
        <Picker resources={resources} held={held} initial={initial} onChange={setScopes} />
      </DialogContent>
      <DialogActions>
        <Button color="inherit" onClick={onClose}>
          Cancel
        </Button>
        <Button variant="contained" onClick={() => onSave(scopes)}>
          Save
        </Button>
      </DialogActions>
    </>
  );
}

/**
 * The logged-in user's new password (`PUT /v1/user`); the server ends every
 * other session and answers this tab's new one.
 */
function Password({
  open,
  onClose,
  onChanged,
}: {
  open: boolean;
  onClose: () => void;
  onChanged: (token: string) => void;
}) {
  const [next, setNext] = useState('');
  const [repeat, setRepeat] = useState('');
  const [problem, setProblem] = useState<string | null>(null);
  const mismatch = repeat !== '' && repeat !== next;

  const close = () => {
    setNext('');
    setRepeat('');
    setProblem(null);
    onClose();
  };

  const submit = (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (next !== repeat) return;
    setProblem(null);
    call(client.PUT('/v1/user', { body: { password: next } }))
      .then((session) => {
        close();
        onChanged(session.token);
      })
      .catch((error: unknown) =>
        setProblem(error instanceof Error ? error.message : String(error)),
      );
  };

  return (
    <Dialog open={open} onClose={close} fullWidth maxWidth="xs">
      <Box component="form" onSubmit={submit}>
        <DialogTitle variant="h3">Change password</DialogTitle>
        <DialogContent>
          <Stack spacing={2} sx={{ pt: 1 }}>
            <TextField
              label="New password"
              type="password"
              autoComplete="new-password"
              value={next}
              onChange={(e) => setNext(e.target.value)}
              autoFocus
              helperText="8 characters at least"
              slotProps={{ htmlInput: { minLength: 8 } }}
              required
            />
            <TextField
              label="Repeat new password"
              type="password"
              autoComplete="new-password"
              value={repeat}
              onChange={(e) => setRepeat(e.target.value)}
              error={mismatch}
              helperText={mismatch ? 'The passwords differ.' : ' '}
              required
            />
            <Typography color="text.secondary" sx={{ fontSize: 12.5 }}>
              Every other session you and your keys issued is signed out; this one stays.
            </Typography>
            {problem && <Alert severity="error">{problem}</Alert>}
          </Stack>
        </DialogContent>
        <DialogActions>
          <Button color="inherit" onClick={close}>
            Cancel
          </Button>
          <Button type="submit" variant="contained" disabled={mismatch}>
            Change
          </Button>
        </DialogActions>
      </Box>
    </Dialog>
  );
}
