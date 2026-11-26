import Add from '@mui/icons-material/Add';
import People from '@mui/icons-material/People';
import Box from '@mui/material/Box';
import IconButton from '@mui/material/IconButton';
import Tooltip from '@mui/material/Tooltip';
import { useCallback, useEffect, useState } from 'react';
import { call, client, openapi, type User, type UserRead } from '@/api/client';
import { Empty } from '@/_components/empty';
import { Side, SideItem } from '@/_components/side';
import { Split } from '@/_components/split';
import type { Shell } from '@/layout';
import { go, useSegments } from '@/route';
import { AddUser } from './_components/add';
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

/** Every user beside the one open (`users/{id}`); the caller holds `users.$read`. */
export function Users({ shell, me, onMe }: { shell: Shell; me: UserRead; onMe: () => void }) {
  const [users, setUsers] = useState<User[] | null>(null);
  const [resources, setResources] = useState<Resources>([]);
  const [adding, setAdding] = useState(false);
  const [, id] = useSegments();
  const held = unpack(me.user.scopes);

  const refresh = useCallback(() => {
    allUsers().then(setUsers).catch(shell.fail);
  }, [shell.fail]);

  useEffect(refresh, [refresh]);

  useEffect(() => {
    openapi()
      .then((document) => setResources(resourcesOf(document)))
      .catch(shell.fail);
  }, [shell.fail]);

  const current = users?.find((u) => u.id === id) ?? null;

  return (
    <Split
      columns="minmax(220px, 280px) minmax(0, 1fr)"
      open={current !== null}
      onClose={() => go(['users'])}
      list={
        <Side
          title="Users"
          {...(users && { count: String(users.length) })}
          empty={users?.length === 0 && 'No user.'}
          actions={
            allows(held, `${ROOT}.users.$write`) && (
              <Tooltip title="Add user">
                <IconButton
                  size="small"
                  aria-label="Add user"
                  onClick={(e) => {
                    e.currentTarget.blur();
                    setAdding(true);
                  }}
                >
                  <Add fontSize="small" />
                </IconButton>
              </Tooltip>
            )
          }
        >
          {users?.map((u) => (
            <SideItem
              key={u.id}
              name={u.id === me.user.id ? `${u.name} (you)` : u.name}
              dot={dotOf(u)}
              detail={u.email}
              selected={u.id === current?.id}
              onClick={() => go(['users', u.id])}
            />
          ))}
          <AddUser
            open={adding}
            shell={shell}
            held={held}
            resources={resources}
            onClose={() => setAdding(false)}
            onCreated={(user) => {
              setAdding(false);
              refresh();
              go(['users', user.id]);
            }}
          />
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
            />
          ) : (
            <Empty icon={<People />} title="Users">
              A user logs in to this dashboard and issues keys for servers. Pick one to see its
              scopes; your own keys, two-factor and session are on your page.
            </Empty>
          )}
        </Box>
      }
    />
  );
}
