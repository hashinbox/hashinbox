import Alert from '@mui/material/Alert';
import { useEffect, useState, type ReactNode } from 'react';
import { Logs, type Entry } from '@/_components/logs';
import { call, client } from '@/api/client';
import type { Shell } from '@/layout';

export function Events({
  chain,
  network,
  component,
  node,
  shell,
  controls,
}: {
  chain: string;
  network: string;
  component: string;
  node: string;
  shell: Shell;
  controls?: ReactNode;
}) {
  const [entries, setEntries] = useState<Entry[]>([]);
  const [gap, setGap] = useState(false);
  useEffect(() => {
    let live = true;
    let after = 0;
    let session: string | null = null;
    let timer: number;
    let retained: Entry[] = [];
    const poll = async () => {
      try {
        const page = await call(
          client.GET('/v1/logs', {
            params: {
              query: {
                ...(chain ? { chain } : {}),
                ...(network ? { network } : {}),
                ...(component ? { component } : {}),
                ...(node ? { node_id: node } : {}),
                after,
                limit: 200,
              },
            },
          }),
        );
        if (!live) return;
        if (session && session !== page.session) {
          retained = [];
          setEntries([]);
          after = 0;
          session = page.session;
          timer = window.setTimeout(() => void poll(), 0);
          return;
        }
        session = page.session;
        if (page.next < after) retained = [];
        after = page.next;
        const received = page.entries.map((e): Entry => ({
          id: e.sequence,
          at: new Date(e.timestamp),
          level: e.level === 'error' ? 'error' : e.level === 'warn' ? 'warn' : 'info',
          source: [
            e.fields.network,
            e.fields.component,
            e.fields.node_id ? `node ${e.fields.node_id}` : null,
          ]
            .filter(Boolean)
            .join(' · '),
          text: `${e.message} ${Object.entries(e.fields)
            .filter(([key]) => !['chain', 'network', 'component', 'node_id'].includes(key))
            .map(([key, value]) => `${key}=${value}`)
            .join(' ')}${e.truncated ? ' [truncated]' : ''}`,
        }));
        retained = [...retained, ...received].slice(-1000);
        setEntries(retained);
        setGap((lost) => lost || page.gap);
      } catch (error) {
        if (live) shell.fail(error);
      }
      if (live) timer = window.setTimeout(() => void poll(), 2000);
    };
    void poll();
    return () => {
      live = false;
      window.clearTimeout(timer);
    };
  }, [chain, network, component, node, shell]);

  return (
    <>
      {gap && <Alert severity="info">Some earlier logs were cleared or evicted.</Alert>}
      <Logs entries={entries} controls={controls} />
    </>
  );
}
