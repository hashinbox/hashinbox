import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Chip from '@mui/material/Chip';
import Stack from '@mui/material/Stack';
import Table from '@mui/material/Table';
import TableBody from '@mui/material/TableBody';
import TableCell from '@mui/material/TableCell';
import TableContainer from '@mui/material/TableContainer';
import TableHead from '@mui/material/TableHead';
import TableRow from '@mui/material/TableRow';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useCallback, useEffect, useState, type SubmitEvent } from 'react';
import { call, client, type Balance, type Transaction, type Wallet as Account } from '@/api/client';
import { Coin } from '@/_components/coin';
import { Copy } from '@/_components/copy';
import { EmptyRow } from '@/_components/table';
import type { Shell } from '@/layout';
import { assetOf, atomic, decimalsOf, effect, format, shorten, unitOf } from '@/money';
import { mono } from '@/theme';
import { Send } from './send';
import { WatchDialog } from './watch';

const PAGE = 100;

/** What a balance says about the history behind it. */
function claimOf(balance: Balance): { text: string; color: string } {
  if (balance.amount.startsWith('-')) {
    return {
      text: 'More left this address than ever reached it: credits are missing.',
      color: 'error.main',
    };
  }
  if (balance.indexed_through == null) {
    return { text: 'Not indexed yet.', color: 'warning.main' };
  }
  if (balance.tip != null && balance.indexed_through < balance.tip) {
    return {
      text: `Indexed through ${balance.indexed_through} of ${balance.tip}; later movements are not counted yet.`,
      color: 'warning.main',
    };
  }
  return { text: `Indexed through ${balance.indexed_through}.`, color: 'text.secondary' };
}

const figures = { fontFamily: mono, fontWeight: 300, fontVariantNumeric: 'tabular-nums' } as const;

export function Wallet({
  shell,
  wallet,
  tags,
  onTagged,
  onWatched,
}: {
  shell: Shell;
  wallet: Account;
  /** This wallet's tags. */
  tags: string[];
  /** Called after a tag was added or removed. */
  onTagged: () => void;
  /** Called with the wallet after it is watched in more assets. */
  onWatched: (wallet: Account) => void;
}) {
  const decimals = decimalsOf(wallet, shell.listings);
  const unit = unitOf(wallet, shell.listings);
  const [balance, setBalance] = useState<Balance | null>(null);
  const [rows, setRows] = useState<Transaction[]>([]);
  const [cursor, setCursor] = useState<string | null>(null);
  const [watchOpen, setWatchOpen] = useState(false);

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
        setRows(page.transactions);
        setCursor(page.next_cursor ?? null);
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
              {wallet.assets.length === 0 ? 'not watched' : wallet.assets.join(', ')} ·{' '}
              {wallet.network.chain}/{wallet.network.name}
              {!wallet.keyed && ' · watch only'}
            </Typography>
            <Button size="small" variant="outlined" onClick={() => setWatchOpen(true)}>
              {wallet.assets.length === 0 ? 'Watch' : 'Watch more assets'}
            </Button>
          </Stack>
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
          <Tags shell={shell} wallet={wallet} tags={tags} onTagged={onTagged} />
        </Box>
        <Box sx={{ textAlign: { md: 'right' } }}>
          <Typography variant="overline" component="p">
            Balance
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
          {claim && (
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
            </Typography>
          )}
        </Box>
      </Box>

      {wallet.keyed && (
        <Send shell={shell} wallet={wallet} onSent={() => window.setTimeout(refresh, 2500)} />
      )}

      <Typography variant="h3" component="h3" sx={{ mt: 3.5, mb: 1 }}>
        Transactions
      </Typography>
      <TableContainer>
        <Table>
          <TableHead>
            <TableRow>
              <TableCell>Outcome</TableCell>
              <TableCell align="right">Block</TableCell>
              <TableCell align="right">Amount</TableCell>
              <TableCell align="right">Fee</TableCell>
              <TableCell>Transaction</TableCell>
            </TableRow>
          </TableHead>
          <TableBody>
            {rows.length === 0 ? (
              <EmptyRow columns={5} message="No transactions yet." />
            ) : (
              rows.map((tx) => (
                <Row key={tx.transaction_id} shell={shell} tx={tx} decimals={decimals} />
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
      <WatchDialog
        shell={shell}
        wallet={wallet}
        open={watchOpen}
        onClose={() => setWatchOpen(false)}
        onDone={(watched) => {
          setWatchOpen(false);
          shell.notify(`Watching ${watched.assets.join(', ')}.`);
          onWatched(watched);
        }}
      />
    </Box>
  );
}

/** The wallet's tags, removable, and a field to add one. */
function Tags({
  shell,
  wallet,
  tags,
  onTagged,
}: {
  shell: Shell;
  wallet: Account;
  tags: string[];
  onTagged: () => void;
}) {
  const [text, setText] = useState('');
  const params = (tag: string) => ({ params: { path: { id: wallet.id, tag } } });
  const add = (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    const tag = text.trim().toLowerCase();
    if (tag === '') return;
    call(client.PUT('/v1/wallets/{id}/tags/{tag}', params(tag)))
      .then(() => {
        setText('');
        onTagged();
      })
      .catch(shell.fail);
  };
  const remove = (tag: string) => {
    call(client.DELETE('/v1/wallets/{id}/tags/{tag}', params(tag)))
      .then(onTagged)
      .catch(shell.fail);
  };
  return (
    <Stack
      component="form"
      direction="row"
      onSubmit={add}
      sx={{ flexWrap: 'wrap', gap: 0.75, alignItems: 'center', mt: 1.5 }}
    >
      {tags.map((tag) => (
        <Chip key={tag} size="small" label={tag} onDelete={() => remove(tag)} />
      ))}
      <TextField
        size="small"
        placeholder="add tag"
        value={text}
        onChange={(e) => setText(e.target.value)}
        slotProps={{
          htmlInput: {
            'aria-label': 'Add tag',
            pattern: '[a-z0-9-]{1,64}',
            spellCheck: false,
            sx: { py: 0.5, fontSize: 12, width: 110 },
          },
        }}
      />
    </Stack>
  );
}

function Row({ shell, tx, decimals }: { shell: Shell; tx: Transaction; decimals: number }) {
  const e = effect(tx, decimals);
  const sign = e.net > 0n ? '+' : e.net < 0n ? '−' : '';
  const color = tx.outcome === 'reverted' ? 'error.main' : 'text.primary';
  return (
    <TableRow hover>
      <TableCell sx={{ whiteSpace: 'nowrap', color }}>
        {tx.outcome}
        {tx.reason && (
          <Box
            component="small"
            sx={{ ...figures, fontSize: 11, color: 'text.secondary', ml: 0.75 }}
          >
            {tx.reason}
          </Box>
        )}
      </TableCell>
      <TableCell align="right" sx={figures}>
        {tx.block.height}
      </TableCell>
      <TableCell align="right" sx={figures}>
        {sign}
        {format(e.net < 0n ? -e.net : e.net, decimals)}
      </TableCell>
      <TableCell align="right" sx={figures}>
        {e.fee > 0n ? format(e.fee, decimals) : ''}
      </TableCell>
      <TableCell sx={{ ...figures, fontSize: 12, whiteSpace: 'nowrap' }}>
        <span title={tx.transaction_id}>{shorten(tx.transaction_id)}</span>{' '}
        <Copy text={tx.transaction_id} label="copy" onDone={shell.notify} />
      </TableCell>
    </TableRow>
  );
}
