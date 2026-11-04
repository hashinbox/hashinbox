import Box from '@mui/material/Box';
import Stack from '@mui/material/Stack';
import Table from '@mui/material/Table';
import TableBody from '@mui/material/TableBody';
import TableCell from '@mui/material/TableCell';
import TableHead from '@mui/material/TableHead';
import TableRow from '@mui/material/TableRow';
import Typography from '@mui/material/Typography';
import { useTheme } from '@mui/material/styles';
import { useCallback, useEffect, useState } from 'react';
import {
  ApiError,
  call,
  client,
  type ChainBlock,
  type ChainTransaction,
  type ConfiguredNetwork,
} from '@/api/client';
import { Address } from '@/_components/address';
import { EmptyRow } from '@/_components/table';
import { Transaction } from '@/_components/transaction';
import { When } from '@/_components/when';
import type { Shell } from '@/layout';
import { Money } from '@/_components/money';
import { shorten } from '@/money';
import { mono } from '@/theme';

const BEAT_MS = 4000;
/** How many blocks are asked for: more than the widest strip shows. */
const BLOCKS = 30;
/** One block's width in the strip: the cube and the line after it. */
const SLOT = 124;
/** By `state`: 0 pending, 1 succeeded, 2 reverted. */
const STATE = ['pending', 'succeeded', 'reverted'];
const TONE = ['warning.main', 'success.main', 'error.main'];

/** The last blocks and the mempool, read side by side. */
interface ChainView {
  blocks: ChainBlock[];
  mempool: ChainTransaction[];
}

/** The view read last, and which of its blocks the read before did not have. */
interface Read {
  view: ChainView;
  fresh: Set<number>;
}

/**
 * The chain as kept: blocks as cubes, newest next to the mempool on the
 * right, older ones fading out to the left; a new block slides in. Below,
 * what the picked block (`picked`), or the mempool (`null`), holds.
 */
export function Chain({
  shell,
  network,
  picked,
  onPick,
}: {
  shell: Shell;
  network: ConfiguredNetwork;
  picked: number | null;
  onPick: (picked: number | null) => void;
}) {
  const [read, setRead] = useState<Read | null>(null);
  const { chain, name } = network.network;

  const refresh = useCallback(() => {
    const path = { chain, network: name };
    Promise.all([
      call(
        client.GET('/v1/networks/{chain}/{network}/blocks', {
          params: { path, query: { limit: BLOCKS } },
        }),
      ),
      call(
        client.GET('/v1/networks/{chain}/{network}/transactions', {
          params: { path, query: { pending: true } },
        }),
      ),
    ])
      .then(([{ blocks }, { transactions: mempool }]) =>
        setRead((last) => {
          const view = { blocks, mempool };
          const known = new Set(last?.view.blocks.map((b) => b.position));
          const fresh = new Set(
            last ? view.blocks.map((b) => b.position).filter((p) => !known.has(p)) : [],
          );
          return { view, fresh };
        }),
      )
      .catch(shell.fail);
  }, [chain, name, shell.fail]);

  useEffect(() => {
    refresh();
    const timer = window.setInterval(refresh, BEAT_MS);
    return () => window.clearInterval(timer);
  }, [refresh]);

  const view = read?.view ?? null;
  const blocks = [...(view?.blocks ?? [])].reverse();
  // The picked block and its first page of transactions; `block: null`: none is kept there.
  const { fail } = shell;
  const [far, setFar] = useState<{
    position: number;
    block: ChainBlock | null;
    transactions: ChainTransaction[];
  } | null>(null);
  useEffect(() => {
    if (picked === null) return;
    let live = true;
    const params = { path: { chain, network: name, position: picked } };
    Promise.all([
      call(client.GET('/v1/networks/{chain}/{network}/blocks/{position}', { params })),
      call(client.GET('/v1/networks/{chain}/{network}/blocks/{position}/transactions', { params })),
    ])
      .then(([found, { transactions }]) => {
        if (live) setFar({ position: picked, block: found, transactions });
      })
      .catch((error: unknown) => {
        if (!live) return;
        if (error instanceof ApiError && error.status === 404)
          setFar({ position: picked, block: null, transactions: [] });
        else fail(error);
      });
    return () => {
      live = false;
    };
  }, [chain, name, picked, fail]);
  const opened = far?.position === picked ? far : null;
  const block = opened?.block ?? null;
  const shown = picked === null ? (view?.mempool ?? []) : (opened?.transactions ?? []);

  return (
    <Box component="article" sx={{ minWidth: 0 }}>
      <Typography variant="body2" color="text.secondary" sx={{ fontFamily: mono, mb: 1 }}>
        indexed #{network.indexed ?? '—'}
        {network.indexed != null &&
          network.tip_live != null &&
          ` · ${String(Math.max(network.tip_live - network.indexed, 0))} behind`}
        {' · '}confirmed #{network.confirmed ?? '—'}
      </Typography>
      <Box
        sx={{
          overflow: 'hidden',
          mb: 2,
          maskImage: 'linear-gradient(to right, transparent 0, #000 240px)',
          '@keyframes enter': { from: { width: 0, opacity: 0 }, to: { width: SLOT, opacity: 1 } },
        }}
      >
        <Stack direction="row" sx={{ alignItems: 'center', justifyContent: 'flex-end' }}>
          {blocks.map((b) => (
            <Stack
              key={b.position}
              direction="row"
              sx={{
                alignItems: 'center',
                flex: 'none',
                width: SLOT,
                overflow: 'hidden',
                justifyContent: 'flex-end',
                animation: read?.fresh.has(b.position) ? 'enter 600ms ease-out' : undefined,
              }}
            >
              <Cube
                label={`#${b.position}`}
                selected={picked === b.position}
                onClick={() => onPick(b.position)}
              />
              <Link />
            </Stack>
          ))}
          <Cube
            label="mempool"
            count={view?.mempool.length ?? 0}
            selected={picked === null}
            pending
            onClick={() => onPick(null)}
          />
        </Stack>
      </Box>

      <Typography variant="h4" component="h2" sx={{ fontFamily: mono, mb: 1 }}>
        {picked === null ? 'Pending transactions' : `Block #${picked}`}
      </Typography>
      {block && <Facts block={block} />}
      <Table size="small">
        <TableHead>
          <TableRow>
            <TableCell>Transaction</TableCell>
            <TableCell>Order</TableCell>
            <TableCell>From</TableCell>
            <TableCell>To</TableCell>
            <TableCell>Amount</TableCell>
            <TableCell>Fee</TableCell>
          </TableRow>
        </TableHead>
        <TableBody>
          {shown.length === 0 && (
            <EmptyRow
              columns={6}
              message={
                picked === null
                  ? 'Nothing pending.'
                  : opened?.block === null
                    ? 'No live block is kept at this position.'
                    : 'No kept transaction.'
              }
            />
          )}
          {shown.map((tx) => {
            const { transfers, fees } = transfersOf(tx.legs);
            const span = Math.max(transfers.length, 1);
            const amount = (asset: string, value: string) => (
              <Amount shell={shell} chain={chain} network={name} asset={asset} amount={value} />
            );
            const side = (address: string | null) =>
              address === null ? (
                <Box component="span" sx={{ color: 'text.disabled' }} title="Not indexed">
                  —
                </Box>
              ) : (
                <Address value={address} network={network.network} />
              );
            const head = (
              <>
                <TableCell rowSpan={span} sx={{ fontSize: 12, whiteSpace: 'nowrap' }}>
                  <Box
                    component="span"
                    aria-label={STATE[tx.state]}
                    sx={{
                      display: 'inline-block',
                      mr: 1,
                      width: 8,
                      height: 8,
                      borderRadius: '50%',
                      bgcolor: TONE[tx.state],
                    }}
                  />
                  <Transaction value={tx.txid} network={network.network} />
                </TableCell>
                <TableCell rowSpan={span} sx={{ fontFamily: mono, fontSize: 12 }}>
                  {tx.order ?? '—'}
                </TableCell>
              </>
            );
            const fee = (
              <TableCell rowSpan={span} sx={{ fontSize: 12 }}>
                {fees.map((f, i) => (
                  <Box key={i}>{amount(f.asset, f.amount)}</Box>
                ))}
              </TableCell>
            );
            if (transfers.length === 0) {
              return (
                <TableRow key={tx.txid}>
                  {head}
                  <TableCell colSpan={3} />
                  {fee}
                </TableRow>
              );
            }
            return transfers.map((t, i) => (
              <TableRow key={`${tx.txid}.${String(i)}`}>
                {i === 0 && head}
                <TableCell sx={{ fontSize: 12 }}>{side(t.from)}</TableCell>
                <TableCell sx={{ fontSize: 12 }}>{side(t.to)}</TableCell>
                <TableCell sx={{ fontSize: 12 }}>{amount(t.asset, t.amount)}</TableCell>
                {i === 0 && fee}
              </TableRow>
            ));
          })}
        </TableBody>
      </Table>
    </Box>
  );
}

type Leg = ChainTransaction['legs'][number];

/** Value moved from one address to another; `null`: that side is not indexed. */
interface Transfer {
  from: string | null;
  to: string | null;
  asset: string;
  amount: string;
}

/**
 * A transaction's legs as transfers: each outgoing leg paired with an
 * incoming one of the same asset and amount; what pairs with nothing kept
 * has an unknown side. Fees apart.
 */
function transfersOf(legs: Leg[]): { transfers: Transfer[]; fees: Leg[] } {
  // kind: 0 received, 1 spent, 2 burned, 3 minted, 4 fee.
  const incoming = legs.filter((l) => l.kind === 0 || l.kind === 3);
  const transfers: Transfer[] = [];
  for (const out of legs.filter((l) => l.kind === 1 || l.kind === 2)) {
    const at = incoming.findIndex((l) => l.asset === out.asset && l.amount === out.amount);
    const to = at >= 0 ? (incoming.splice(at, 1)[0] ?? null) : null;
    transfers.push({
      from: out.address,
      to: to?.address ?? null,
      asset: out.asset,
      amount: out.amount,
    });
  }
  for (const l of incoming) {
    transfers.push({ from: null, to: l.address, asset: l.asset, amount: l.amount });
  }
  return { transfers, fees: legs.filter((l) => l.kind === 4) };
}

/** An atomic amount of `asset`; an unlisted asset by its id. */
function Amount({
  shell,
  chain,
  network,
  asset,
  amount,
}: {
  shell: Shell;
  chain: string;
  network: string;
  asset: string;
  amount: string;
}) {
  const listing = shell.listings.find(
    (l) => l.asset === asset && l.network.chain === chain && l.network.name === network,
  );
  if (!listing) return `${amount} ${shorten(asset)}`;
  return (
    <Money
      amount={amount}
      chain={chain}
      ticker={listing.metadata.ticker}
      decimals={listing.metadata.decimals}
      display={listing.metadata.display_decimals}
    />
  );
}

function Facts({ block }: { block: ChainBlock }) {
  return (
    <Typography sx={{ fontFamily: mono, fontSize: 12, color: 'text.secondary', mb: 1 }}>
      {shorten(block.hash)}
      {typeof block.timestamp === 'number' && (
        <>
          {' · '}
          <When at={block.timestamp * 1000} />
        </>
      )}
    </Typography>
  );
}

/** The line between two cubes. */
function Link() {
  return <Box sx={{ width: 28, height: 2, bgcolor: 'divider', flex: 'none' }} />;
}

/** One block (or the mempool) as an isometric cube with its label and, for the mempool, transaction count. */
function Cube({
  label,
  count,
  selected,
  pending,
  onClick,
}: {
  label: string;
  count?: number;
  selected: boolean;
  pending?: boolean;
  onClick: () => void;
}) {
  const theme = useTheme();
  const base = pending ? theme.palette.warning.main : theme.palette.primary.main;
  const stroke = selected ? theme.palette.text.primary : 'transparent';
  return (
    <Box
      component="button"
      type="button"
      aria-label={label}
      aria-pressed={selected}
      onClick={onClick}
      sx={{ p: 0, border: 0, bgcolor: 'transparent', cursor: 'pointer', flex: 'none' }}
    >
      <svg width="96" height="104" viewBox="0 0 96 104" role="img" aria-hidden="true">
        <polygon
          points="48,4 92,26 48,48 4,26"
          fill={base}
          fillOpacity={0.55}
          stroke={stroke}
          strokeWidth={1.5}
        />
        <polygon
          points="4,26 48,48 48,100 4,78"
          fill={base}
          fillOpacity={0.9}
          stroke={stroke}
          strokeWidth={1.5}
        />
        <polygon
          points="92,26 48,48 48,100 92,78"
          fill={base}
          fillOpacity={0.7}
          stroke={stroke}
          strokeWidth={1.5}
        />
        <text
          x="26"
          y="64"
          textAnchor="middle"
          fontFamily={mono}
          fontSize={label.length > 8 ? 7.5 : 9}
          fill={theme.palette.getContrastText(base)}
          transform="skewY(26.5) translate(0,-13)"
        >
          {label}
        </text>
        <text
          x="70"
          y="64"
          textAnchor="middle"
          fontFamily={mono}
          fontSize={13}
          fill={theme.palette.getContrastText(base)}
          transform="skewY(-26.5) translate(0,35)"
        >
          {count}
        </text>
      </svg>
    </Box>
  );
}
