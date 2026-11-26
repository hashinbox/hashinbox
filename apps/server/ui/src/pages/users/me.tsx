import Box from '@mui/material/Box';
import { useEffect, useState } from 'react';
import type { UserRead } from '@hashinbox/client';
import { openapi } from '@/api/client';
import type { Shell } from '@/layout';
import { resourcesOf, type Resources } from './_components/scopes';
import { UserDetail, type Own } from './_components/user';

/** The logged-in user's own page (`GET /v1/user`), whatever its scopes. */
export function Me({
  shell,
  me,
  onMe,
  own,
}: {
  shell: Shell;
  me: UserRead;
  /** The logged-in user changed: read it again. */
  onMe: () => void;
  own: Own;
}) {
  const [resources, setResources] = useState<Resources>([]);

  useEffect(() => {
    openapi()
      .then((document) => setResources(resourcesOf(document)))
      .catch(shell.fail);
  }, [shell.fail]);

  return (
    <Box sx={{ px: { xs: 2, md: 4 }, py: { xs: 2.5, md: 3.5 }, minWidth: 0 }}>
      <UserDetail
        shell={shell}
        me={me}
        user={me.user}
        resources={resources}
        onChanged={onMe}
        own={own}
      />
    </Box>
  );
}
