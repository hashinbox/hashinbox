import { EmptyRow } from '@/_components/table';
import Box from '@mui/material/Box';
import FormControlLabel from '@mui/material/FormControlLabel';
import Switch from '@mui/material/Switch';
import Tooltip from '@mui/material/Tooltip';
import Button from '@mui/material/Button';
import Stack from '@mui/material/Stack';
import Table from '@mui/material/Table';
import TableBody from '@mui/material/TableBody';
import TableCell from '@mui/material/TableCell';
import TableContainer from '@mui/material/TableContainer';
import TableHead from '@mui/material/TableHead';
import TableRow from '@mui/material/TableRow';
import Typography from '@mui/material/Typography';
import { useCallback, useEffect, useState } from 'react';
import { call, client, type Balance, type Transaction, type Wallet as Account } from '@/api/client';
import { Coin } from '@/_components/coin';
import { Confirm } from '@/_components/confirm';
import { Copy } from '@/_components/copy';
import type { Shell } from '@/layout';
import { assetOf, atomic, decimalsOf, effect, format, shorten, unitOf } from '@/money';
import { mono } from '@/theme';
import { Send } from './send';

const PAGE = 100;

/** What a balance says about the history behind it. */
function claimOf(balance: Balance): { text: string; color: string } {
  if (balance.amount.startsWith('-')) {
    return {
      text: 'More left this address than ever reached it: credits are missing.',
      color: 'error.main',
    };
  }
  if (balance.behind > 0) {
    return {
      text: `History is ${balance.behind} block(s) behind; earlier movements may be missing.`,
      color: 'warning.main',
    };
  }
  return { text: 'History is read up to the confirmed line.', color: 'text.secondary' };
}

const STATUS: Record<Transaction['status']['kind'], string> = {
  confirmed: 'text.primary',
  pending: 'warning.main',
  failed: 'error.main',
};

const figures = { fontFamily: mono, fontWeight: 300, fontVariantNumeric: 'tabular-nums' } as const;

/** When the wallet was archived, as local date text. */
function day(seconds: number): string {
  return new Date(seconds * 1000).toLocaleDateString();
}

export function Wallet({
  shell,
  wallet,
  onRetired,
}: {
  shell: Shell;
  wallet: Account;
  onRetired: (wallet: Account) => void;
}) {
  const decimals = decimalsOf(wallet, shell.listings);
  const unit = unitOf(wallet, shell.listings);
  const [balance, setBalance] = useState<Balance | null>(null);
  const [rows, setRows] = useState<Transaction[]>([]);
  const [pending, setPending] = useState(0);
  const [cursor, setCursor] = useState<string | null>(null);
  const [line, setLine] = useState<number | null>(null);
  // Not wired yet: the API has no route to stop or restart a watch.
  const [watching, setWatching] = useState(true);
  const [archiving, setArchiving] = useState(false);
  const live = wallet.archived_at == null;

  const archive = () => {
    setArchiving(false);
    call(client.DELETE('/v1/wallets/{id}', { params: { path: { id: wallet.id } } }))
      .then((archived) => {
        shell.notify('Wallet archived. Its address is no longer watched.');
        onRetired(archived);
      })
      .catch(shell.fail);
  };

  const refresh = useCallback(() => {
    const id = { path: { id: wallet.id } };
    call(
      client.GET('/v1/wallets/{id}/balance', {
        params: { ...id, query: { asset: assetOf(wallet) } },
      }),
    )
      .then(setBalance)
      .catch(shell.fail);
    call(
      client.GET('/v1/wallets/{id}/transactions', {
        params: { ...id, query: { asset: assetOf(wallet), limit: PAGE } },
      }),
    )
      .then((page) => {
        setRows([...page.pending, ...page.transactions]);
        setPending(page.pending.length);
        setCursor(page.next_cursor ?? null);
        setLine(page.checkpoint?.height ?? null);
      })
      .catch(shell.fail);
  }, [wallet, shell.fail]);

  useEffect(refresh, [refresh]);

  const older = () => {
    if (!cursor) return;
    const params = {
      path: { id: wallet.id },
      query: { asset: assetOf(wallet), limit: PAGE, cursor },
    };
    call(client.GET('/v1/wallets/{id}/transactions', { params }))
      .then((page) => {
        setRows((current) => [...current, ...page.transactions]);
        setCursor(page.next_cursor ?? null);
      })
      .catch(shell.fail);
  };

  const claim = balance ? claimOf(balance) : null;
  const incoming = balance ? atomic(balance.pending_incoming, decimals) : 0n;
  const outgoing = balance ? atomic(balance.pending_outgoing, decimals) : 0n;

  return (
    <Box component="article">
      <Box
        sx={{
          display: 'grid',
          gridTemplateColumns: { xs: 'minmax(0, 1fr)', md: 'minmax(0, 1fr) auto' },
          gap: 3,
          pb: 3,
          mb: 2.5,
          borderBottom: 1,
          borderColor: 'divider',
        }}
      >
        <Box sx={{ minWidth: 0 }}>
          <Stack direction="row" spacing={1.25} sx={{ alignItems: 'center' }}>
            <Coin symbol={assetOf(wallet)} size={26} />
            <Typography variant="overline" component="p" sx={{ flexGrow: 1 }}>
              {assetOf(wallet)} · {wallet.network.chain}/{wallet.network.name}
              {wallet.watch_only && ' · watch only'}
              {wallet.archived_at != null && ` · archived ${day(wallet.archived_at)}`}
            </Typography>
            {live && (
              <Button size="small" color="error" onClick={() => setArchiving(true)}>
                Archive
              </Button>
            )}
            {live && (
              <Tooltip title="Stops inspecting new blocks for this address; its history is kept. Not wired to the server yet.">
                <FormControlLabel
                  control={
                    <Switch
                      size="small"
                      checked={watching}
                      onChange={(e) => setWatching(e.target.checked)}
                    />
                  }
                  label={watching ? 'Watching' : 'Paused'}
                  slotProps={{ typography: { sx: { fontSize: 12, color: 'text.secondary' } } }}
                  sx={{ mr: 0 }}
                />
              </Tooltip>
            )}
          </Stack>
          {wallet.label != null && (
            <Typography variant="h2" component="h2" sx={{ mt: 0.75, overflowWrap: 'anywhere' }}>
              {wallet.label}
            </Typography>
          )}
          <Typography sx={{ fontFamily: mono, fontSize: 17, overflowWrap: 'anywhere', my: 0.5 }}>
            {wallet.address}
          </Typography>
          <Stack
            direction="row"
            spacing={1.5}

            sx={{ alignItems: 'baseline', ...figures, fontSize: 12, color: 'text.secondary' }}
          >
            <span>{wallet.id}</span>
            <Copy text={wallet.id} label="copy id" onDone={shell.notify} />
            <Copy text={wallet.address} label="copy address" onDone={shell.notify} />
          </Stack>
        </Box>
        <Box sx={{ textAlign: { md: 'right' } }}>
          <Typography variant="overline" component="p">
            Confirmed
          </Typography>
          <Typography
            variant="h1"
            component="p"
            sx={{ whiteSpace: 'nowrap', fontVariantNumeric: 'tabular-nums' }}
          >
            {balance ? format(atomic(balance.amount, decimals), decimals) : '—'}{' '}
            <Box component="span" sx={{ fontSize: 22, color: 'text.secondary' }}>
              {unit}
            </Box>
          </Typography>
          <Stack
            direction="row"
            spacing={2}
            sx={{
              justifyContent: { md: 'flex-end' },
              ...figures,
              fontSize: 12,
              mt: 1,
              minHeight: 18,
            }}
          >
            {incoming > 0n && <span>+{format(incoming, decimals)} arriving</span>}
            {outgoing > 0n && (
              <Box component="span" sx={{ color: 'text.secondary' }}>
                −{format(outgoing, decimals)} leaving
              </Box>
            )}
          </Stack>
          {claim && balance && (
            <Typography
              sx={{
                fontSize: 12,
                color: claim.color,
                mt: 0.75,
                maxWidth: '42ch',
                ml: { md: 'auto' },
              }}
            >
              {claim.text}
              {balance.observed_height != null && ` Read at block ${balance.observed_height}.`}
            </Typography>
          )}
        </Box>
      </Box>

      {!wallet.watch_only && live && (
        <Send shell={shell} wallet={wallet} onSent={() => window.setTimeout(refresh, 2500)} />
      )}

      <Stack
        direction="row"

        sx={{
          alignItems: 'baseline',
          justifyContent: 'space-between',
          flexWrap: 'wrap',
          mt: 3.5,
          mb: 1,
        }}
      >
        <Typography variant="h3" component="h3">
          Transactions
        </Typography>
        <Typography sx={{ ...figures, fontSize: 12, color: 'text.secondary' }}>
          {line != null ? `confirmed line at ${line}` : 'not synced yet'}
          {pending > 0 && ` · ${pending} pending`}
        </Typography>
      </Stack>
      <TableContainer>
        <Table>
          <TableHead>
            <TableRow>
              <TableCell>Status</TableCell>
              <TableCell align="right">Block</TableCell>
              <TableCell align="right">Amount</TableCell>
              <TableCell>Counterparty</TableCell>
              <TableCell align="right">Fee</TableCell>
              <TableCell>Transaction</TableCell>
            </TableRow>
          </TableHead>
          <TableBody>
            {rows.length === 0 ? (
              <EmptyRow columns={6} message="No transactions yet." />
            ) : (
              rows.map((tx) => (
                <Row
                  key={tx.transaction_id}
                  shell={shell}
                  wallet={wallet}
                  tx={tx}
                  decimals={decimals}
                />
              ))
            )}
          </TableBody>
        </Table>
      </TableContainer>
      {cursor && (
        <Button onClick={older} sx={{ mt: 1.5 }}>
          Older
        </Button>
      )}
      <Confirm
        open={archiving}
        title="Archive wallet"
        phrase={wallet.label ?? wallet.address}
        action="Archive"
        onClose={() => setArchiving(false)}
        onConfirm={archive}
      >
        Stops watching this address and refuses sends from it. Its indexed history and balance stay
        readable. There is no unarchive.
      </Confirm>
    </Box>
  );
}

function Row({
  shell,
  wallet,
  tx,
  decimals,
}: {
  shell: Shell;
  wallet: Account;
  tx: Transaction;
  decimals: number;
}) {
  const e = effect(wallet, tx, decimals);
  const s = tx.status;
  const sign = e.net > 0n ? '+' : e.net < 0n ? '−' : '';
  return (
    <TableRow hover>
      <TableCell sx={{ whiteSpace: 'nowrap', color: STATUS[s.kind] }}>
        <Box
          component="i"
          sx={{
            display: 'inline-block',
            width: 8,
            height: 8,
            borderRadius: '50%',
            mr: 1,
            border: 1,
            borderColor: STATUS[s.kind],
            bgcolor: s.kind === 'pending' ? 'transparent' : STATUS[s.kind],
          }}
        />
        {s.kind}
        <Box component="small" sx={{ ...figures, fontSize: 11, color: 'text.secondary', ml: 0.75 }}>
          {s.kind === 'failed' ? s.reason : `${s.confirmations}×`}
        </Box>
      </TableCell>
      <TableCell align="right" sx={figures}>
        {s.block.height}
      </TableCell>
      <TableCell align="right" sx={figures}>
        {sign}
        {format(e.net < 0n ? -e.net : e.net, decimals)}
      </TableCell>
      <TableCell sx={{ ...figures, fontSize: 12 }}>
        {e.counterparties.length === 0
          ? '—'
          : e.counterparties.slice(0, 2).map((a) => (
              <div key={a} title={a}>
                {shorten(a)}
              </div>
            ))}
        {e.counterparties.length > 2 && <div>+{e.counterparties.length - 2}</div>}
      </TableCell>
      <TableCell align="right" sx={figures}>
        {e.fee && `${e.fee.amount} ${e.fee.asset.ticker ?? e.fee.asset.id}`}
      </TableCell>
      <TableCell sx={{ ...figures, fontSize: 12, whiteSpace: 'nowrap' }}>
        <span title={tx.transaction_id}>{shorten(tx.transaction_id)}</span>{' '}
        <Copy text={tx.transaction_id} label="copy" onDone={shell.notify} />
      </TableCell>
    </TableRow>
  );
}
