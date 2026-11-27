import ShowChart from '@mui/icons-material/ShowChart';
import Box from '@mui/material/Box';
import { useCallback, useEffect, useState } from 'react';
import { marketsList, type MarketList } from '@hashinbox/client';
import { api } from '@/api/client';
import { Empty } from '@/_components/empty';
import { Split } from '@/_components/split';
import { When } from '@/_components/when';
import { go, useSegments } from '@/route';
import { Side, SideItem } from '@/_components/side';
import { Market } from './_components/market';
import { fail } from '@/store/app';

const BEAT_MS = 10_000;

/** The markets beside the one whose candles and logs are shown. */
export function Markets() {
  const [list, setList] = useState<MarketList | null>(null);
  // markets/{name}
  const [, selected] = useSegments();

  const refresh = useCallback(() => {
    marketsList(api).then(setList).catch(fail);
  }, []);

  useEffect(() => {
    refresh();
    const timer = window.setInterval(refresh, BEAT_MS);
    return () => window.clearInterval(timer);
  }, [refresh]);

  const markets = list?.markets ?? [];
  const current = markets.find((m) => m.name === selected) ?? null;
  const tickers = Object.keys(list?.prices ?? {}).sort();

  return (
    <Split
      columns="minmax(220px, 280px) minmax(0, 1fr)"
      open={current !== null}
      onClose={() => go(['markets'])}
      list={
        <Side
          title="Markets"
          empty={list !== null && markets.length === 0 && 'No market is configured.'}
        >
          {markets.map((m) => (
            <SideItem
              key={m.name}
              name={m.name}
              dot={{
                color: m.feed ? 'success.main' : 'error.main',
                label: m.feed ? 'Price feed on' : 'Price feed off',
              }}
              selected={m.name === current?.name}
              onClick={() => go(['markets', m.name])}
              detail={
                m.last_at === null ? (
                  'no quote'
                ) : (
                  <>
                    last quote <When at={m.last_at} />
                  </>
                )
              }
            />
          ))}
        </Side>
      }
      detail={
        <Box sx={{ px: { xs: 2, md: 4 }, py: { xs: 2.5, md: 3.5 }, minWidth: 0 }}>
          {current ? (
            <Market key={current.name} market={current} tickers={tickers} onChanged={refresh} />
          ) : (
            <Empty icon={<ShowChart />} title="Markets">
              A market is a price source this server asks for quotes. Pick one to see its hourly
              candles over the last 7 days, its price feed switch and its logs.
            </Empty>
          )}
        </Box>
      }
    />
  );
}
