import Alert from '@mui/material/Alert';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Checkbox from '@mui/material/Checkbox';
import Dialog from '@mui/material/Dialog';
import DialogActions from '@mui/material/DialogActions';
import DialogContent from '@mui/material/DialogContent';
import DialogTitle from '@mui/material/DialogTitle';
import FormControlLabel from '@mui/material/FormControlLabel';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useState, type SubmitEvent } from 'react';
import { usersCreate, type User } from '@hashinbox/client';
import { ApiError, Reason, api } from '@/api/client';
import { mono } from '@/theme';
import { Picker } from './picker';
import { pack, short, type Resources } from './scopes';
import { notify } from '@/store/app';

/** 64 symbols: a byte's low 6 bits pick one without bias. */
const ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_';

/** 20 random symbols, 120 bits. */
function generated(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(20));
  return Array.from(bytes, (b) => ALPHABET[b & 63]).join('');
}

/** What the create answer's failure means to the admin. */
function problemOf(error: unknown): string {
  const failure = ApiError.from(error);
  if (failure.kind === 'already_exists') return 'A user with this email already exists.';
  const scope = failure.metadata.scope;
  if (failure.reason === Reason.MissingScope && scope) return `You can't grant ${short(scope)}.`;
  return failure.message;
}

/**
 * A new user (`POST /v1/users`): email, name, a typed or generated password,
 * scopes from the caller's; a generated password is shown once after.
 */
export function AddUser({
  open,
  held,
  resources,
  onClose,
  onCreated,
}: {
  open: boolean;
  held: string[];
  resources: Resources;
  onClose: () => void;
  onCreated: (user: User) => void;
}) {
  const [shown, setShown] = useState<{ user: User; password: string } | null>(null);
  return (
    <>
      <Dialog open={open} onClose={onClose} fullWidth maxWidth="sm">
        <Create
          resources={resources}
          held={held}
          onClose={onClose}
          onCreated={(user, password) => {
            onCreated(user);
            if (password) setShown({ user, password });
            else notify(`${user.name} added.`);
          }}
        />
      </Dialog>
      <Dialog open={shown !== null} fullWidth maxWidth="sm">
        <DialogTitle variant="h3">{shown?.user.name} added</DialogTitle>
        <DialogContent>
          <Alert severity="warning" sx={{ mb: 2 }}>
            Copy the password now: it is shown this once. Hand it over safely and ask{' '}
            {shown?.user.email} to change it on /user.
          </Alert>
          <Box
            sx={{
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
            {shown?.password}
          </Box>
        </DialogContent>
        <DialogActions>
          <Button
            onClick={() => {
              navigator.clipboard.writeText(shown?.password ?? '').then(
                () => notify('Password copied.'),
                () => notify('Copy failed: select the password and copy it by hand.', true),
              );
            }}
          >
            Copy password
          </Button>
          <Button variant="contained" onClick={() => setShown(null)}>
            Done
          </Button>
        </DialogActions>
      </Dialog>
    </>
  );
}

function Create({
  resources,
  held,
  onClose,
  onCreated,
}: {
  resources: Resources;
  held: string[];
  onClose: () => void;
  /** With the password when it was generated. */
  onCreated: (user: User, password: string | null) => void;
}) {
  const [email, setEmail] = useState('');
  const [name, setName] = useState('');
  const [generate, setGenerate] = useState(false);
  const [password, setPassword] = useState('');
  const [repeat, setRepeat] = useState('');
  const [scopes, setScopes] = useState<string[]>([]);
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const mismatch = !generate && repeat !== '' && repeat !== password;

  const submit = (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (!generate && password !== repeat) return;
    setBusy(true);
    setProblem(null);
    const secret = generate ? generated() : password;
    const body = { email: email.trim(), name: name.trim(), password: secret, scopes: pack(scopes) };
    usersCreate(api, { body })
      .then((created) => onCreated(created.user, generate ? secret : null))
      .catch((error: unknown) => setProblem(problemOf(error)))
      .finally(() => setBusy(false));
  };

  return (
    <Box component="form" onSubmit={submit}>
      <DialogTitle variant="h3">Add user</DialogTitle>
      <DialogContent>
        <Stack spacing={2} sx={{ pt: 1 }}>
          <TextField
            label="Email"
            type="email"
            autoComplete="off"
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            autoFocus
            required
          />
          <TextField
            label="Name"
            value={name}
            onChange={(e) => setName(e.target.value)}
            slotProps={{ htmlInput: { maxLength: 128 } }}
            required
          />
          <FormControlLabel
            label="Generate a password"
            control={
              <Checkbox checked={generate} onChange={(e) => setGenerate(e.target.checked)} />
            }
          />
          {!generate && (
            <>
              <TextField
                label="Initial password"
                type="password"
                autoComplete="new-password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                helperText="8 characters at least"
                slotProps={{ htmlInput: { minLength: 8 } }}
                required
              />
              <TextField
                label="Repeat password"
                type="password"
                autoComplete="new-password"
                value={repeat}
                onChange={(e) => setRepeat(e.target.value)}
                error={mismatch}
                helperText={mismatch ? 'The passwords differ.' : ' '}
                required
              />
            </>
          )}
          <Picker resources={resources} held={held} initial={[]} onChange={setScopes} />
          {scopes.length === 0 && (
            <Typography color="text.secondary" sx={{ fontSize: 12.5 }}>
              No scope: the user can only see its own /user page.
            </Typography>
          )}
          {problem && <Alert severity="error">{problem}</Alert>}
        </Stack>
      </DialogContent>
      <DialogActions>
        <Button color="inherit" onClick={onClose}>
          Cancel
        </Button>
        <Button type="submit" variant="contained" disabled={busy || mismatch}>
          Add
        </Button>
      </DialogActions>
    </Box>
  );
}
