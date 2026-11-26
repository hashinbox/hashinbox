import Box from '@mui/material/Box';
import Typography from '@mui/material/Typography';
import { useCallback, useEffect, useState, type ReactNode } from 'react';
import { call, client, type Schemas } from '@/api/client';
import { HashinboxMark } from '@/_components/logo';
import { Sparkline } from '@/_components/sparkline';
import { Stat } from '@/_components/stat';
import type { Shell } from '@/layout';

type Stats = Schemas['Stats'];

const BEAT_MS = 10_000;
const compact = new Intl.NumberFormat(undefined, { notation: 'compact' });

/** An hour's start as a local time. */
function hourOf(start: number): string {
  return new Date(start * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

/** What the store holds across every network, and its last day, in cards; `banner` above. */
export function Dashboard({ shell, banner }: { shell: Shell; banner?: ReactNode }) {
  const [stats, setStats] = useState<Stats | null>(null);

  const refresh = useCallback(() => {
    call(client.GET('/v1/stats')).then(setStats).catch(shell.fail);
  }, [shell.fail]);

  useEffect(() => {
    refresh();
    const timer = window.setInterval(refresh, BEAT_MS);
    return () => window.clearInterval(timer);
  }, [refresh]);

  return (
    <Box sx={{ px: { xs: 2, md: 4 }, py: { xs: 2.5, md: 3.5 }, position: 'relative', zIndex: 0 }}>
      <Watermark />
      <Typography variant="h2" component="h1" sx={{ mb: 2.5 }}>
        Dashboard
      </Typography>
      {banner}
      {stats && <Cards stats={stats} />}
    </Box>
  );
}

/** The mark, large and faint, fixed behind the cards in the bottom-right corner. */
function Watermark() {
  return (
    <Box
      aria-hidden
      sx={{
        position: 'fixed',
        right: '4vmin',
        bottom: '4vmin',
        width: '50vmin',
        height: '50vmin',
        zIndex: -1,
        pointerEvents: 'none',
        color: 'text.primary',
      }}
    >
      {/* Faint as a whole, so the surface needs a bigger step to show: 0.08 box, 0.04 triangles. */}
      <HashinboxMark opacity={0.08} surface={0.5} style={{ width: '100%', height: '100%' }} />
    </Box>
  );
}

function Cards({ stats }: { stats: Stats }) {
  const chainLabels = stats.chain.map((h) => hourOf(h.start));
  const serverLabels = stats.server.map((h) => hourOf(h.start));
  const sum = (values: number[]) => values.reduce((a, b) => a + b, 0);
  const trend = (label: string, values: number[], labels: string[], unit: string) => (
    <Sparkline
      label={label}
      values={values}
      labels={labels}
      format={(v) => `${String(v)} ${unit}`}
    />
  );
  const blocks = stats.chain.map((h) => h.blocks);
  const transactions = stats.chain.map((h) => h.transactions);
  const succeeded = stats.server.map((h) => h.succeeded);
  const failed = stats.server.map((h) => h.failed);
  const runs = stats.server.map((h) => h.runs);
  return (
    <Box
      sx={{ display: 'grid', gap: 2, gridTemplateColumns: 'repeat(auto-fill, minmax(240px, 1fr))' }}
    >
      <Stat
        label="Transactions indexed"
        value={compact.format(stats.transactions)}
        note={`${String(sum(transactions))} in the last 24 h · ${String(stats.reverted)} reverted`}
        trend={trend('Transactions per hour', transactions, chainLabels, 'transactions')}
      />
      <Stat
        label="Blocks kept"
        value={compact.format(stats.blocks)}
        note={`${String(sum(blocks))} in the last 24 h`}
        trend={trend('Blocks per hour', blocks, chainLabels, 'blocks')}
      />
      <Stat label="Pending" value={compact.format(stats.pending)} note="Sent, not mined yet" />
      <Stat
        label="Movements"
        value={compact.format(stats.movements)}
        note="Transfers, fees and rewards of indexed addresses"
      />
      <Stat
        label="Addresses indexed"
        value={compact.format(stats.addresses)}
        note={`${String(stats.wallets)} wallets with a key · ${String(stats.tags)} tags`}
      />
      <Stat
        label="Triggers"
        value={`${String(stats.enabled)} of ${String(stats.triggers)}`}
        note={`enabled · ${String(sum(runs))} runs in the last 24 h`}
        trend={trend('Trigger runs per hour', runs, serverLabels, 'runs')}
      />
      <Stat
        label="Jobs succeeded · last 24 h"
        value={compact.format(sum(succeeded))}
        note={`${String(stats.jobs.pending)} pending · ${String(stats.jobs.running)} running`}
        trend={trend('Jobs succeeded per hour', succeeded, serverLabels, 'jobs')}
      />
      <Stat
        label="Jobs failed · last 24 h"
        value={compact.format(sum(failed))}
        note={`${String(stats.jobs.failed)} failed in all`}
        trend={trend('Jobs failed per hour', failed, serverLabels, 'jobs')}
      />
    </Box>
  );
}
