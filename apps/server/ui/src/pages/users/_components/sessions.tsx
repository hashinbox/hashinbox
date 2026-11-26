import Button from '@mui/material/Button';
import { useEffect, useState } from 'react';
import { sessionGet, type SessionRead } from '@hashinbox/client';
import { api } from '@/api/client';
import { When } from '@/_components/when';
import type { Shell } from '@/layout';
import { ScopeList } from './picker';
import { unpack } from './scopes';
import { Row, Section } from './section';

/** This tab's session: when it was issued, when it ends, what it may do; log out. */
export function Sessions({
  shell,
  onLogout,
}: {
  shell: Shell;
  onLogout: (everywhere: boolean) => void;
}) {
  const [session, setSession] = useState<SessionRead | null>(null);

  useEffect(() => {
    sessionGet(api).then(setSession).catch(shell.fail);
  }, [shell.fail]);

  return (
    <Section
      title="Session"
      actions={
        <>
          <Button onClick={() => onLogout(false)}>Log out</Button>
          <Button color="error" onClick={() => onLogout(true)}>
            Log out everywhere
          </Button>
        </>
      }
    >
      {session && (
        <>
          <Row label="Issued">
            <When at={session.issued_at * 1000} />
          </Row>
          <Row label="Expires">
            <When at={session.expires_at * 1000} />
          </Row>
          <Row label="Scopes">
            <ScopeList scopes={unpack(session.scopes)} />
          </Row>
        </>
      )}
    </Section>
  );
}
