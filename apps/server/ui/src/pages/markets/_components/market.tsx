import Box from '@mui/material/Box';
import FormControlLabel from '@mui/material/FormControlLabel';
import Stack from '@mui/material/Stack';
import Switch from '@mui/material/Switch';
import Typography from '@mui/material/Typography';
import { useEffect, useState } from 'react';
import {
  marketCandlesList,
  marketUpdate,
  type MarketCandle1h as Candle,
  type MarketListing,
} from '@hashinbox/client';
import { api } from '@/api/client';
import { LineChart } from '@/_components/chart';
import { LogsButton } from '@/_components/logs/button';
import type { Shell } from '@/layout';
import { mono } from '@/theme';

const BEAT_MS = 10_000;
const WEEK_MS = 7 * 86_400_000;

/** A USD price as `$97,123.45`; under a dollar, up to six decimals. */
function usd(value: number): string {
  return `$${value.toLocaleString(undefined, { maximumFractionDigits: value >= 1 ? 2 : 6 })}`;
}

/** `held` and `fresh` by hour, `fresh` winning, none before `since` (Unix ms), oldest first. */
function merged(held: Candle[], fresh: Candle[], since: number): Candle[] {
  const byStart = new Map(held.map((c) => [c.start, c]));
  for (const c of fresh) byStart.set(c.start, c);
  return [...byStart.values()]
    .filter((c) => c.start * 1000 >= since)
    .sort((a, b) => a.start - b.start);
}

/** A market: its price feed switch, its candles of each of `tickers` over the last 7 days, its logs. */
export function Market({
  shell,
  market,
  tickers,
  onChanged,
}: {
  shell: Shell;
  market: MarketListing;
  tickers: string[];
  onChanged: () => void;
}) {
  const toggle = () => {
    marketUpdate(api, { name: market.name, body: { feed: !market.feed } })
      .then(() => {
        shell.notify(`${market.name} price feed ${market.feed ? 'off' : 'on'}.`);
        onChanged();
      })
      .catch(shell.fail);
  };

  return (
    <Box component="article">
      <Stack direction="row" sx={{ alignItems: 'center', mb: 2 }}>
        <Typography variant="h2" component="h1" sx={{ fontFamily: mono, flexGrow: 1 }}>
          {market.name}
        </Typography>
        <FormControlLabel
          control={<Switch checked={market.feed} onChange={toggle} />}
          label="Price feed"
        />
        <LogsButton filter={{ market: market.name }} />
      </Stack>
      <Box
        sx={{
          display: 'grid',
          gridTemplateColumns: 'repeat(auto-fill, minmax(min(100%, 360px), 1fr))',
          gap: 2,
        }}
      >
        {tickers.map((ticker) => (
          <Candles key={ticker} shell={shell} market={market.name} asset={ticker} />
        ))}
      </Box>
    </Box>
  );
}

/**
 * The market's hourly closes of `asset` over the last 7 days; read again
 * every 10 s from the last hour on (its candle still moves).
 */
function Candles({ shell, market, asset }: { shell: Shell; market: string; asset: string }) {
  const [held, setHeld] = useState<Candle[]>([]);
  const [now, setNow] = useState(Date.now);

  useEffect(() => {
    let live = true;
    let after: number | undefined;
    const read = () => {
      marketCandlesList(api, { name: market, asset, ...(after !== undefined && { after }) })
        .then((fresh) => {
          if (!live) return;
          after = fresh.candles.at(-1)?.start ?? after;
          const at = Date.now();
          setNow(at);
          setHeld((was) => merged(was, fresh.candles, at - WEEK_MS));
        })
        .catch(shell.fail);
    };
    read();
    const timer = window.setInterval(read, BEAT_MS);
    return () => {
      live = false;
      window.clearInterval(timer);
    };
  }, [market, asset, shell.fail]);

  return (
    <Box sx={{ minWidth: 0, p: 1.5, border: 1, borderColor: 'divider', borderRadius: 1 }}>
      <Stack direction="row" sx={{ justifyContent: 'space-between', mb: 0.5 }}>
        <Typography sx={{ fontFamily: mono, fontSize: 13 }}>{asset}</Typography>
        <Typography sx={{ fontSize: 13, fontVariantNumeric: 'tabular-nums' }}>
          {held.length > 0 ? usd(Number(held[held.length - 1]?.close)) : ''}
        </Typography>
      </Stack>
      {held.length === 0 ? (
        <Typography color="text.secondary" sx={{ fontSize: 13 }}>
          No candle in the last 7 days.
        </Typography>
      ) : (
        <LineChart
          label={`${market} ${asset} hourly USD close, last 7 days`}
          series={held.map((c) => ({ at: c.start * 1000, value: Number(c.close) }))}
          from={Math.max(now - WEEK_MS, (held[0]?.start ?? 0) * 1000 - 3_600_000)}
          to={now}
          format={usd}
        />
      )}
    </Box>
  );
}
