import Alert from '@mui/material/Alert';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Link from '@mui/material/Link';
import Typography from '@mui/material/Typography';
import { useEffect, useRef, useState } from 'react';
import { call, client, type Schemas } from '@/api/client';
import { Confirm } from '@/_components/confirm';
import type { Shell } from '@/layout';
import { go, href } from '@/route';
import { NewPassphrase } from './passphrase';
import { Row, Section } from './section';

/** The section's anchor, which the dashboard's banner links to. */
const ANCHOR = 'recovery';
/** Where this browser remembers the banner was dismissed. */
const DISMISSED_KEY = 'hashinbox.recovery.dismissed';

/**
 * The organization's recovery passphrase (`/v1/recovery`): whether it is set,
 * and create, replace or remove it. Shown to a holder of `recovery.$write`.
 */
export function Recovery({ shell }: { shell: Shell }) {
  const [state, setState] = useState<Schemas['Recovery'] | null>(null);
  const [busy, setBusy] = useState(false);
  const [replacing, setReplacing] = useState(false);
  const [removing, setRemoving] = useState(false);
  const [words, setWords] = useState<string[] | null>(null);
  const anchor = useRef<HTMLDivElement>(null);

  useEffect(() => {
    call(client.GET('/v1/recovery')).then(setState).catch(shell.fail);
  }, [shell.fail]);

  // Linked from the dashboard's banner: scrolled to once shown.
  useEffect(() => {
    if (state && window.location.hash === `#${ANCHOR}`)
      anchor.current?.scrollIntoView({ behavior: 'smooth' });
  }, [state]);

  // Set locally: a read with a session the old words made would answer 401.
  const create = () => {
    setBusy(true);
    call(client.POST('/v1/recovery'))
      .then((created) => {
        setWords(created.passphrase.split(/\s+/).filter(Boolean));
        setState({ set: true, created_at: Date.now() });
      })
      .catch(shell.fail)
      .finally(() => setBusy(false));
  };

  const remove = () => {
    call(client.DELETE('/v1/recovery'))
      .then(() => {
        setState({ set: false, created_at: null });
        shell.notify('Recovery passphrase removed: passphrase sign-in is off.');
      })
      .catch(shell.fail);
  };

  const set = state?.set === true;

  return (
    <Box ref={anchor} id={ANCHOR}>
      <Section
        title="Recovery passphrase"
        actions={
          state && (
            <>
              <Button disabled={busy} onClick={set ? () => setReplacing(true) : create}>
                {busy ? 'Creating…' : set ? 'Replace' : 'Create'}
              </Button>
              {set && (
                <Button color="error" disabled={busy} onClick={() => setRemoving(true)}>
                  Remove
                </Button>
              )}
            </>
          )
        }
      >
        <Row label="State">
          {state === null
            ? '…'
            : set && state.created_at != null
              ? `Set since ${new Date(state.created_at).toLocaleDateString()}`
              : 'Not set'}
        </Row>
        <Typography color="text.secondary" sx={{ fontSize: 13, mt: 1 }}>
          With its 12 words anyone can sign in to any account without two-factor. Keep them offline.
        </Typography>
      </Section>

      <Confirm
        open={replacing}
        title="Replace the recovery passphrase"
        phrase="replace"
        action="Replace"
        onClose={() => setReplacing(false)}
        onConfirm={() => {
          setReplacing(false);
          create();
        }}
      >
        The old words stop working, and anyone signed in with them is signed out.
      </Confirm>
      <Confirm
        open={removing}
        title="Remove the recovery passphrase"
        phrase="remove"
        action="Remove"
        onClose={() => setRemoving(false)}
        onConfirm={() => {
          setRemoving(false);
          remove();
        }}
      >
        Passphrase sign-in is off until a new one is created, and anyone signed in with the words is
        signed out.
      </Confirm>
      {words && (
        <NewPassphrase key={words.join(' ')} words={words} onClose={() => setWords(null)} />
      )}
    </Box>
  );
}

function dismissed(): boolean {
  try {
    return localStorage.getItem(DISMISSED_KEY) !== null;
  } catch {
    return false;
  }
}

/**
 * The dashboard's dismissible nudge while no recovery passphrase is set
 * (`GET /v1/recovery`); for a holder of `recovery.$write`.
 */
export function RecoveryBanner() {
  const [shown, setShown] = useState(false);

  useEffect(() => {
    if (dismissed()) return;
    let live = true;
    call(client.GET('/v1/recovery'))
      .then((state) => {
        if (live) setShown(!state.set);
      })
      .catch(() => undefined);
    return () => {
      live = false;
    };
  }, []);

  if (!shown) return null;
  const target = `${href(['user'])}#${ANCHOR}`;
  return (
    <Alert
      severity="warning"
      sx={{ mb: 2.5 }}
      onClose={() => {
        setShown(false);
        try {
          localStorage.setItem(DISMISSED_KEY, '1');
        } catch {
          /* private window */
        }
      }}
    >
      No recovery passphrase yet.{' '}
      <Link
        href={target}
        onClick={(e) => {
          e.preventDefault();
          go(['user']);
          window.history.replaceState(null, '', target);
        }}
      >
        Create one
      </Link>
    </Alert>
  );
}
