import Box from '@mui/material/Box';
import { useCallback, useEffect, useMemo, useState } from 'react';
import { call, client, events, type StreamEvent } from '@/api/client';
import type { Shell } from '@/layout';
import { Logs as Entries } from '@/_components/logs';
import { valueOf, type Tag } from '@/_components/tag';
import { Tags } from '@/_components/tags';

const LEVELS = ['error', 'warn', 'info', 'debug', 'trace'] as const;

/** One tag of the filter: a network, a level or an event. */
type Filter = Tag<'network' | 'level' | 'target'>;

/** Every kept log entry, newest first, and each new one live; filtered. */
export function Logs({ shell }: { shell: Shell }) {
  const [tags, setTags] = useState<Filter[]>([]);
  // The events kept so far, as the server lists them.
  const [targets, setTargets] = useState<string[]>([]);
  useEffect(() => {
    call(client.GET('/v1/logs/targets'))
      .then((list) => setTargets(list.targets))
      .catch(shell.fail);
  }, [shell.fail]);
  const level = valueOf(tags, 'level');
  const target = valueOf(tags, 'target');
  const network = valueOf(tags, 'network');

  const query = useMemo(
    () => ({ ...(level ? { level } : {}), ...(target ? { target } : {}) }),
    [level, target],
  );
  const read = useCallback(
    (cursor: string | undefined) => {
      const page = { ...query, ...(cursor !== undefined && { cursor }) };
      const [chain, name] = network.split('/');
      if (chain && name)
        return call(
          client.GET('/v1/networks/{chain}/{network}/logs', {
            params: { path: { chain, network: name }, query: page },
          }),
        );
      return call(client.GET('/v1/logs', { params: { query: page } }));
    },
    [query, network],
  );
  const path = `/v1/logs/stream?${new URLSearchParams({
    ...query,
    ...(network ? { network } : {}),
  }).toString()}`;
  const listen = useCallback(
    (onEvent: (event: StreamEvent) => void, signal: AbortSignal) => events(path, onEvent, signal),
    [path],
  );
  const networks = [
    ...new Set(shell.listings.map((l) => `${l.network.chain}/${l.network.name}`)),
  ].sort();
  const options: Filter[] = [
    ...networks.map((n) => ({ group: 'network' as const, value: n, label: n })),
    ...LEVELS.map((l) => ({ group: 'level' as const, value: l, label: `${l} and above` })),
    ...targets.map((t) => ({ group: 'target' as const, value: t, label: t })),
  ];

  return (
    <Box sx={{ px: { xs: 2, md: 4 }, py: { xs: 2.5, md: 3.5 }, minWidth: 0 }}>
      <Tags
        options={options}
        value={tags}
        onChange={setTags}
        placeholder="Filter by network, level or event"
        label="Filter logs"
      />
      <Entries key={path} shell={shell} read={read} listen={listen} />
    </Box>
  );
}
