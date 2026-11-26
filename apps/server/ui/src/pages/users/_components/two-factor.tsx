import Alert from '@mui/material/Alert';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useState, type SubmitEvent } from 'react';
import {
  twoFactorConfirm,
  twoFactorStart,
  twoFactorStop,
  type TwoFactor as State,
} from '@hashinbox/client';
import { ApiError, api } from '@/api/client';
import { Copy } from '@/_components/copy';
import { When } from '@/_components/when';
import type { Shell } from '@/layout';
import { mono } from '@/theme';
import { Row, Section } from './section';

/** The secret an `otpauth://` URI carries, for typing into an app by hand. */
function secretOf(uri: string): string {
  try {
    return new URL(uri).searchParams.get('secret') ?? '';
  } catch {
    return '';
  }
}

/** What a wrong code reads as; anything else, the server's own message. */
function problemOf(error: unknown): string {
  if (error instanceof ApiError && error.status === 401) return 'The code is wrong.';
  return error instanceof Error ? error.message : String(error);
}

/**
 * The logged-in user's two-factor: off, pending (started, no code yet) or
 * on. Turning it on shows the authenticator setup, then asks a first code;
 * turning it off asks a code and ends every session.
 */
export function TwoFactor({
  shell,
  state,
  onChanged,
  onSignedOut,
}: {
  shell: Shell;
  state: State | null;
  onChanged: () => void;
  onSignedOut: () => void;
}) {
  const [uri, setUri] = useState<string | null>(null);
  const [code, setCode] = useState('');
  const [problem, setProblem] = useState<string | null>(null);
  const on = state?.confirmed_at != null;

  const start = () => {
    setProblem(null);
    twoFactorStart(api)
      .then((started) => {
        setUri(started.uri);
        onChanged();
      })
      .catch(shell.fail);
  };

  const submit = (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    setProblem(null);
    const body = { code: code.trim() };
    if (on) {
      twoFactorStop(api, { body })
        .then(() => {
          shell.notify('Two-factor off: every session ended. Log in again.');
          onSignedOut();
        })
        .catch((error: unknown) => setProblem(problemOf(error)));
      return;
    }
    twoFactorConfirm(api, { body })
      .then(() => {
        setUri(null);
        setCode('');
        shell.notify('Two-factor on: a login asks for a code from now.');
        onChanged();
      })
      .catch((error: unknown) => setProblem(problemOf(error)));
  };

  const codeForm = (action: string, color: 'primary' | 'error') => (
    <Stack
      component="form"
      direction="row"
      spacing={1.5}
      sx={{ alignItems: 'flex-start', mt: 2, maxWidth: 360 }}
      onSubmit={submit}
    >
      <TextField
        label="6-digit code"
        value={code}
        onChange={(e) => setCode(e.target.value)}
        autoComplete="one-time-code"
        slotProps={{ htmlInput: { inputMode: 'numeric', pattern: '[0-9]{6}', maxLength: 6 } }}
        required
      />
      <Button type="submit" variant="contained" color={color} sx={{ flex: 'none', mt: 0.5 }}>
        {action}
      </Button>
    </Stack>
  );

  return (
    <Section
      title="Two-factor"
      actions={
        !on && (
          <Button onClick={start}>
            {state === null && uri === null ? 'Turn on' : 'Start again'}
          </Button>
        )
      }
    >
      <Row label="State">
        {on ? (
          <>
            On since <When at={state.confirmed_at ?? 0} />
          </>
        ) : state ? (
          <>
            Pending: started <When at={state.created_at} />, no code confirmed yet
          </>
        ) : (
          'Off'
        )}
      </Row>
      {uri && (
        <Box sx={{ mt: 2 }}>
          <Typography sx={{ fontSize: 13 }}>
            Add this account to your authenticator app: open the link on a phone that has one, or
            type the secret into the app.
          </Typography>
          <Row label="Secret">
            <Box component="span" sx={{ fontFamily: mono, wordBreak: 'break-all', mr: 1 }}>
              {secretOf(uri)}
            </Box>
            <Copy text={secretOf(uri)} label="copy" onDone={shell.notify} />
          </Row>
          <Row label="otpauth URI">
            <Box
              component="a"
              href={uri}
              sx={{ fontFamily: mono, wordBreak: 'break-all', color: 'text.primary', mr: 1 }}
            >
              {uri}
            </Box>
            <Copy text={uri} label="copy" onDone={shell.notify} />
          </Row>
          <Typography color="text.secondary" sx={{ fontSize: 12.5, mt: 1 }}>
            Then enter the code it shows to turn two-factor on.
          </Typography>
          {codeForm('Confirm', 'primary')}
        </Box>
      )}
      {!uri && state && !on && (
        <Typography color="text.secondary" sx={{ fontSize: 13, mt: 1 }}>
          Start again to see the secret, then confirm a code.
        </Typography>
      )}
      {on && (
        <>
          <Typography color="text.secondary" sx={{ fontSize: 13, mt: 1 }}>
            Turning it off needs a code, and ends every session you and your keys issued.
          </Typography>
          {codeForm('Turn off', 'error')}
        </>
      )}
      {problem && (
        <Alert severity="error" sx={{ mt: 2, maxWidth: 360 }}>
          {problem}
        </Alert>
      )}
    </Section>
  );
}
