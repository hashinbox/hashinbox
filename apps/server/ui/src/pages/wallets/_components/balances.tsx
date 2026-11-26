import InfoOutlined from '@mui/icons-material/InfoOutlined';
import Box from '@mui/material/Box';
import Tooltip from '@mui/material/Tooltip';
import type { ReactNode } from 'react';
import type { Wallet } from '@hashinbox/client';
import { format } from '@/_components/format';
import { mono } from '@/theme';

type Index = Wallet['indexes'][string];

/** Why a balance of `index` reads null. */
function missingOf(index: Index): string {
  if (index.indexed_through == null) return 'Not indexed yet: no block has been read.';
  if (index.finality_at == null) return "The network's final block is not known yet.";
  return 'Not known yet.';
}

/** A label with an info icon that explains it in `info`. */
function Label({ text, info }: { text: string; info: string }) {
  return (
    <Box
      component="dt"
      sx={{ display: 'flex', alignItems: 'center', gap: 0.5, color: 'text.secondary' }}
    >
      {text}
      <Tooltip title={info} placement="top" arrow>
        <InfoOutlined sx={{ fontSize: 14, color: 'text.disabled', cursor: 'help' }} />
      </Tooltip>
    </Box>
  );
}

/** Every balance of one wallet index, with how far it is indexed. */
export function Balances({
  index,
  units,
}: {
  index: Index;
  units: { decimals: number; display: number | null; ticker: string };
}) {
  const missing = missingOf(index);

  const amount = (value: string | null | undefined, info: string): [ReactNode, string] => {
    if (value == null) return ['—', `${info} ${missing}`];
    return [
      <>
        {format(value, units)}{' '}
        <Box component="span" sx={{ color: 'text.secondary', fontSize: 11 }}>
          {units.ticker}
        </Box>
      </>,
      info,
    ];
  };
  const usd = (value: string | null | undefined, info: string): [ReactNode, string] =>
    value == null ? ['—', `${info} ${missing}`] : [`$${format(value, { display: 2 })}`, info];
  const position = (value: number | null | undefined, info: string, none: string) =>
    value == null ? (['—', `${info} ${none}`] as const) : ([String(value), info] as const);

  const rows: [string, ReactNode, string][] = [
    [
      'Confirmed',
      ...amount(
        index.balance_confirmed,
        'What it holds in final blocks: the total after its last leg at or below the final block.',
      ),
    ],
    [
      'Spendable',
      ...amount(
        index.balance_confirmed_spendable,
        'What it can spend now: confirmed less outgoing transactions that are pending or not final yet.',
      ),
    ],
    [
      'Pending',
      ...amount(index.balance_pending, 'What it will hold once every pending transaction lands.'),
    ],
    [
      'Received',
      ...amount(
        index.balance_confirmed_received,
        'Everything it had received in final blocks; confirmed is received less sent plus adjustments.',
      ),
    ],
    [
      'Sent',
      ...amount(
        index.balance_confirmed_sent,
        'Everything it had sent in final blocks, fees included.',
      ),
    ],
    [
      'Received USD',
      ...usd(
        index.balance_confirmed_received_usd,
        'Received in dollars, each leg at its price when it was kept; a leg without a price counts 0.',
      ),
    ],
    [
      'Sent USD',
      ...usd(
        index.balance_confirmed_sent_usd,
        'Sent in dollars, each leg at its price when it was kept; a leg without a price counts 0.',
      ),
    ],
    [
      'Indexed through',
      ...position(
        index.indexed_through,
        'The last block read for this asset.',
        'No block read yet.',
      ),
    ],
    [
      'Behind tip',
      ...position(
        index.behind_tip,
        "Blocks between the last one read and the network's newest.",
        'No block read yet, or the tip is unknown.',
      ),
    ],
    [
      'Final at',
      ...position(
        index.finality_at,
        "The network's final block: nothing at or below it can be undone.",
        'Not known yet.',
      ),
    ],
  ];

  return (
    <Box
      component="dl"
      aria-label="Balances"
      sx={{
        m: 0,
        mt: 1.5,
        p: 1.5,
        border: 1,
        borderColor: 'divider',
        borderRadius: '8px',
        display: 'grid',
        gridTemplateColumns: 'repeat(auto-fill, minmax(170px, 1fr))',
        gap: 1.5,
        fontSize: 12,
      }}
    >
      {rows.map(([label, value, info]) => (
        <Box key={label} sx={{ minWidth: 0 }}>
          <Label text={label} info={info} />
          <Box
            component="dd"
            sx={{
              m: 0,
              fontFamily: mono,
              fontSize: 14,
              fontVariantNumeric: 'tabular-nums',
              whiteSpace: 'nowrap',
              overflow: 'hidden',
              textOverflow: 'ellipsis',
            }}
          >
            {value}
          </Box>
        </Box>
      ))}
    </Box>
  );
}
