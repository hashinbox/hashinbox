import People from '@mui/icons-material/People';
import Box from '@mui/material/Box';
import { useCallback, useEffect, useState } from 'react';
import { call, client, openapi, type User, type UserRead } from '@/api/client';
import { Empty } from '@/_components/empty';
import { Side, SideItem } from '@/_components/side';
import { Split } from '@/_components/split';
import type { Shell } from '@/layout';
import { go, useSegments } from '@/route';
import { ROOT, allows, resourcesOf, unpack, type Resources } from './_components/scopes';
import { UserDetail } from './_components/user';

/** Every user, page by page. */
async function allUsers(): Promise<User[]> {
  const users: User[] = [];
  let cursor: string | undefined;
  do {
    const query = { limit: 500, ...(cursor !== undefined && { cursor }) };
    const page = await call(client.GET('/v1/users', { params: { query } }));
    users.push(...page.users);
    cursor = page.next_cursor ?? undefined;
  } while (cursor !== undefined);
  return users;
}

/** Green active, red disabled, grey without a dashboard login. */
function dotOf(user: User): { color: string; label: string } {
  if (user.disabled_at != null) return { color: 'error.main', label: 'Disabled' };
  if (!user.login) return { color: 'text.disabled', label: 'No dashboard login' };
  return { color: 'success.main', label: 'Active' };
}

/**
 * The users beside the one open; `users/me` is the logged-in user's own
 * page, reachable without `users.$read`.
 */
export function Users({
  shell,
  me,
  onMe,
  onLogout,
  onSignedOut,
  onSession,
}: {
  shell: Shell;
  me: UserRead;
  /** The logged-in user changed: read it again. */
  onMe: () => void;
  onLogout: (everywhere: boolean) => void;
  /** The server ended every session already (password changed, 2FA off). */
  onSignedOut: () => void;
  /** A fresh session for this tab (the password changed). */
  onSession: (token: string) => void;
}) {
  const [listed, setListed] = useState<User[] | null>(null);
  const [resources, setResources] = useState<Resources>([]);
  // users/{id | me}
  const [, picked] = useSegments();
  const reads = allows(unpack(me.user.scopes), `${ROOT}.users.$read`);

  const refresh = useCallback(() => {
    if (reads) allUsers().then(setListed).catch(shell.fail);
  }, [reads, shell.fail]);

  useEffect(refresh, [refresh]);

  useEffect(() => {
    openapi()
      .then((document) => setResources(resourcesOf(document)))
      .catch(shell.fail);
  }, [shell.fail]);

  const users = reads ? (listed ?? []) : [me.user];
  const id = picked === 'me' ? me.user.id : picked;
  const current = users.find((u) => u.id === id) ?? (id === me.user.id ? me.user : null);

  return (
    <Split
      columns="minmax(220px, 280px) minmax(0, 1fr)"
      open={current !== null}
      onClose={() => go(['users'])}
      list={
        <Side
          title="Users"
          {...(reads && { count: String(users.length) })}
          empty={reads && listed !== null && users.length === 0 && 'No user.'}
        >
          {users.map((u) => (
            <SideItem
              key={u.id}
              name={u.id === me.user.id ? `${u.name} (you)` : u.name}
              dot={dotOf(u)}
              detail={u.email}
              selected={u.id === current?.id}
              onClick={() => go(['users', u.id === me.user.id ? 'me' : u.id])}
            />
          ))}
        </Side>
      }
      detail={
        <Box sx={{ px: { xs: 2, md: 4 }, py: { xs: 2.5, md: 3.5 }, minWidth: 0 }}>
          {current ? (
            <UserDetail
              key={current.id}
              shell={shell}
              me={me}
              user={current}
              resources={resources}
              onChanged={() => {
                refresh();
                onMe();
              }}
              onLogout={onLogout}
              onSignedOut={onSignedOut}
              onSession={onSession}
            />
          ) : (
            <Empty icon={<People />} title="Users">
              A user logs in to this dashboard and issues keys for servers. Pick one to see its
              scopes; your own page also holds your keys, two-factor and session.
            </Empty>
          )}
        </Box>
      }
    />
  );
}
