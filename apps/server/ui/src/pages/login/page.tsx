import Alert from '@mui/material/Alert';
import Button from '@mui/material/Button';
import Link from '@mui/material/Link';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import { useState, type SubmitEvent } from 'react';
import { HashinboxClient, ownUserUpdate } from '@hashinbox/client';
import { ApiError, Reason, api } from '@/api/client';
import { Welcome } from '@/_components/welcome';
import { settings } from '@/settings';
import { mono } from '@/theme';

const MIN = 8;

function problemOf(error: unknown): string {
  return ApiError.from(error).message;
}

/**
 * The passphrase session's own client: it only sets the new password, so
 * the dashboard does not open with it (`api`'s session is the store's).
 */
function recovery(): HashinboxClient {
  return new HashinboxClient(settings.apiBase, { checkScopes: false });
}

/**
 * Email and password, or (forgot password) the recovery passphrase, then
 * the two-factor code when the server asks for it (`totp_required`); hands
 * the session's JWT to `onSession`. After a passphrase sign-in a new
 * password is asked first.
 */
export function Login({
  instance,
  onSession,
}: {
  /** The name the scene shows. */
  instance: string;
  onSession: (token: string) => void;
}) {
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [passphrase, setPassphrase] = useState('');
  const [recovering, setRecovering] = useState(false);
  const [totp, setTotp] = useState('');
  const [asksCode, setAsksCode] = useState(false);
  const [resetting, setResetting] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [client] = useState(recovery);
  const wrong = recovering
    ? 'email, passphrase or code is wrong'
    : 'email, password or code is wrong';

  const submit = (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    setBusy(true);
    setProblem(null);
    const secret = recovering
      ? { passphrase: passphrase.trim().split(/\s+/).join(' ') }
      : { password };
    const login = { email: email.trim(), ...secret, ...(asksCode && { totp: totp.trim() }) };
    // `api` keeps the session, and the dashboard opens; a passphrase's waits for the new password.
    (recovering ? client : api)
      .login(login)
      .then((session) => {
        if (!recovering) {
          onSession(session.token);
          return;
        }
        // A passphrase session: it sets the new password before the dashboard opens.
        setResetting(true);
      })
      .catch((error: unknown) => {
        const failure = ApiError.from(error);
        if (failure.reason === Reason.TotpRequired) {
          setAsksCode(true);
          return;
        }
        if (failure.reason === Reason.WrongCredentials) setProblem(wrong);
        else setProblem(failure.message);
      })
      .finally(() => setBusy(false));
  };

  if (resetting) return <NewPassword client={client} instance={instance} onSession={onSession} />;

  return (
    <Welcome
      name={instance}
      overline={recovering ? 'Forgot password' : 'Log in'}
      title={asksCode ? 'Two-factor code' : recovering ? 'Recovery passphrase' : 'Welcome back'}
      note={
        asksCode
          ? 'Enter the 6-digit code your authenticator app shows.'
          : recovering
            ? "Your email and the organization's 12-word recovery passphrase. You set a new password next."
            : 'The session stays in this tab only.'
      }
    >
      <Stack component="form" spacing={2} onSubmit={submit}>
        {!asksCode && (
          <>
            <TextField
              label="Email"
              type="email"
              autoComplete="username"
              value={email}
              onChange={(e) => setEmail(e.target.value)}
              autoFocus
              required
            />
            {recovering ? (
              <TextField
                label="Recovery passphrase"
                value={passphrase}
                onChange={(e) => setPassphrase(e.target.value)}
                multiline
                minRows={3}
                autoComplete="off"
                slotProps={{ htmlInput: { spellCheck: false, style: { fontFamily: mono } } }}
                required
              />
            ) : (
              <TextField
                label="Password"
                type="password"
                autoComplete="current-password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                required
              />
            )}
          </>
        )}
        {asksCode && (
          <TextField
            label="Code"
            value={totp}
            onChange={(e) => setTotp(e.target.value)}
            autoComplete="one-time-code"
            slotProps={{ htmlInput: { inputMode: 'numeric', pattern: '[0-9]{6}', maxLength: 6 } }}
            autoFocus
            required
          />
        )}
        {problem && <Alert severity="error">{problem}</Alert>}
        <Stack direction="row" spacing={1} sx={{ justifyContent: 'flex-end' }}>
          {asksCode && (
            <Button
              color="inherit"
              onClick={() => {
                setAsksCode(false);
                setTotp('');
                setProblem(null);
              }}
            >
              Back
            </Button>
          )}
          <Button type="submit" variant="contained" disabled={busy}>
            {asksCode ? 'Verify' : 'Log in'}
          </Button>
        </Stack>
        {!asksCode && (
          <Link
            component="button"
            type="button"
            color="text.secondary"
            sx={{ alignSelf: 'flex-start', fontSize: 13 }}
            onClick={() => {
              setRecovering(!recovering);
              setProblem(null);
            }}
          >
            {recovering ? 'Log in with a password' : 'Forgot password?'}
          </Link>
        )}
      </Stack>
    </Welcome>
  );
}

/**
 * Right after a passphrase sign-in: the new password (`PUT /v1/user`), whose
 * answer is the session the dashboard keeps.
 */
function NewPassword({
  client,
  instance,
  onSession,
}: {
  /** Holds the passphrase session. */
  client: HashinboxClient;
  instance: string;
  onSession: (token: string) => void;
}) {
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
    ownUserUpdate(client, { body: { password } })
      .then((session) => onSession(session.token))
      .catch((error: unknown) => setProblem(problemOf(error)))
      .finally(() => setBusy(false));
  };

  return (
    <Welcome
      name={instance}
      overline="Forgot password"
      title="Set a new password"
      note="Signed in with the recovery passphrase. Your other sessions end."
    >
      <Stack component="form" spacing={2} onSubmit={submit}>
        <TextField
          label="New password"
          type="password"
          autoComplete="new-password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          helperText={`${MIN} characters at least`}
          slotProps={{ htmlInput: { minLength: MIN } }}
          autoFocus
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
        {problem && <Alert severity="error">{problem}</Alert>}
        <Stack direction="row" sx={{ justifyContent: 'flex-end' }}>
          <Button type="submit" variant="contained" disabled={busy || mismatch}>
            Continue
          </Button>
        </Stack>
      </Stack>
    </Welcome>
  );
}
