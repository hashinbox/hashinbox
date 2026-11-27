import Alert from '@mui/material/Alert';
import Button from '@mui/material/Button';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import { useState, type ReactNode, type SubmitEvent } from 'react';
import { settingUpdate, usersCreate } from '@hashinbox/client';
import { ApiError, api } from '@/api/client';
import { Welcome } from '@/_components/welcome';

const MIN = 8;

function problemOf(error: unknown): string {
  return ApiError.from(error).message;
}

/**
 * The first run, two steps: the root user (`POST /v1/users`, no session
 * needed while no user exists), then the organization's name (`instance`).
 * `onDone` opens the dashboard.
 */
export function Setup({
  instance,
  onSession,
  onInstance,
  onDone,
}: {
  /** The name the scene shows. */
  instance: string;
  /** The root's session, from step 1. */
  onSession: (token: string) => void;
  /** The organization's new name. */
  onInstance: (name: string) => void;
  onDone: () => void;
}) {
  const [named, setNamed] = useState(false);

  if (!named)
    return (
      <Account
        instance={instance}
        onSession={(token) => {
          onSession(token);
          setNamed(true);
        }}
      />
    );
  return (
    <Organization
      instance={instance}
      onInstance={(name) => {
        onInstance(name);
        onDone();
      }}
    />
  );
}

/** The setup scene, `n of 2` above the title. */
function Frame({
  instance,
  n,
  title,
  note,
  children,
}: {
  instance: string;
  n: number;
  title: string;
  note: ReactNode;
  children: ReactNode;
}) {
  return (
    <Welcome name={instance} overline={`Setup · ${n} of 2`} title={title} note={note}>
      {children}
    </Welcome>
  );
}

/** The form's error and its Continue button. */
function Footer({ problem, disabled }: { problem: string | null; disabled: boolean }) {
  return (
    <>
      {problem && <Alert severity="error">{problem}</Alert>}
      <Stack direction="row" spacing={1} sx={{ justifyContent: 'flex-end' }}>
        <Button type="submit" variant="contained" disabled={disabled}>
          Continue
        </Button>
      </Stack>
    </>
  );
}

/** Step 1: the root user, logged in at once. */
function Account({
  instance,
  onSession,
}: {
  instance: string;
  onSession: (token: string) => void;
}) {
  const [email, setEmail] = useState('');
  const [name, setName] = useState('');
  const [password, setPassword] = useState('');
  const [repeat, setRepeat] = useState('');
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const mismatch = repeat !== '' && repeat !== password;

  const submit = (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (password !== repeat) return;
    setBusy(true);
    setProblem(null);
    const body = { email: email.trim(), name: name.trim(), password };
    usersCreate(api, { body })
      .then((created) => {
        if (created.token) onSession(created.token);
        else setProblem('The user was created, but no session came back. Log in.');
      })
      .catch((error: unknown) => setProblem(problemOf(error)))
      .finally(() => setBusy(false));
  };

  return (
    <Frame
      instance={instance}
      n={1}
      title="Create the root user"
      note="No user exists yet. This one holds every scope and is logged in at once."
    >
      <Stack component="form" spacing={2} onSubmit={submit}>
        <TextField
          label="Email"
          type="email"
          autoComplete="username"
          value={email}
          onChange={(e) => setEmail(e.target.value)}
          autoFocus
          required
        />
        <TextField
          label="Name"
          autoComplete="name"
          value={name}
          onChange={(e) => setName(e.target.value)}
          slotProps={{ htmlInput: { maxLength: 128 } }}
          required
        />
        <TextField
          label="Password"
          type="password"
          autoComplete="new-password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          helperText={`${MIN} characters at least`}
          slotProps={{ htmlInput: { minLength: MIN } }}
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
        <Footer problem={problem} disabled={busy || mismatch} />
      </Stack>
    </Frame>
  );
}

/** Step 2: the organization's name (`PUT /v1/settings/instance`). */
function Organization({
  instance,
  onInstance,
}: {
  instance: string;
  onInstance: (name: string) => void;
}) {
  const [value, setValue] = useState(instance);
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    const name = value.trim();
    setBusy(true);
    setProblem(null);
    settingUpdate(api, { key: 'instance', body: { value: name } })
      .then(() => onInstance(name))
      .catch((error: unknown) => setProblem(problemOf(error)))
      .finally(() => setBusy(false));
  };

  return (
    <Frame instance={instance} n={2} title="Organization" note="What this instance is called.">
      <Stack component="form" spacing={2} onSubmit={submit}>
        <TextField
          label="Organization"
          value={value}
          onChange={(e) => setValue(e.target.value)}
          helperText="Shown in the top-left corner of the dashboard."
          slotProps={{ htmlInput: { minLength: 1, maxLength: 64 } }}
          autoFocus
          required
        />
        <Footer problem={problem} disabled={busy || value.trim() === ''} />
      </Stack>
    </Frame>
  );
}
