import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import ButtonGroup from '@mui/material/ButtonGroup';
import Chip from '@mui/material/Chip';
import Dialog from '@mui/material/Dialog';
import DialogActions from '@mui/material/DialogActions';
import DialogContent from '@mui/material/DialogContent';
import DialogTitle from '@mui/material/DialogTitle';
import Link from '@mui/material/Link';
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
import { Address } from '@/_components/address';
import { Currency } from '@/_components/currency';
import { format } from '@/_components/format';
import { Money } from '@/_components/money';
import { EmptyRow } from '@/_components/table';
import { Transaction as Tx } from '@/_components/transaction';
import type { Shell } from '@/layout';
import { assetName, assetOf, decimalsOf, displayOf, effect, unitOf } from '@/money';
import { go, href } from '@/route';
import { mono } from '@/theme';
import { Send } from './send';
import { WatchDialog } from './watch';
import { haltOf, progressOf } from './watching';

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
  const display = displayOf(wallet, shell.listings);
  const [balance, setBalance] = useState<Balance | null>(null);
  const [rows, setRows] = useState<Transaction[]>([]);
  const [cursor, setCursor] = useState<string | null>(null);
  const [watchOpen, setWatchOpen] = useState(false);
  const [tagOpen, setTagOpen] = useState(false);

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
  const halt = haltOf(wallet);

  /** Pauses every index of the wallet, or resumes them when one is stopped. */
  const toggle = () => {
    const index_state = halt ? 'indexing' : 'paused';
    Promise.all(
      wallet.indexes.map(({ asset }) =>
        call(
          client.PATCH('/v1/wallets/{id}/indexes/{asset}', {
            params: { path: { id: wallet.id, asset } },
            body: { index_state },
          }),
        ),
      ),
    )
      .then(() => call(client.GET('/v1/wallets/{id}', { params: { path: { id: wallet.id } } })))
      .then((changed) => {
        shell.notify(halt ? 'Indexing resumed.' : 'Indexing paused.');
        onWatched(changed);
      })
      .catch(shell.fail);
  };

  return (
    <Box component="article">
      <Box sx={{ pb: 2, mb: 2.5, borderBottom: 1, borderColor: 'divider' }}>
        <Stack
          direction="row"
          spacing={1.5}
          useFlexGap
          sx={{ alignItems: 'center', flexWrap: 'wrap', rowGap: 1 }}
        >
          <Box sx={{ display: 'flex', fontSize: 22 }}>
            <Currency ticker={unit} label={false} />
          </Box>
          <Box sx={{ minWidth: 0 }}>
            <Box sx={{ fontSize: 15 }}>
              <Address value={wallet.address} network={wallet.network} trim={false} />
            </Box>
            <Typography
              variant="overline"
              component="p"
              sx={{ whiteSpace: 'nowrap', lineHeight: 1.6 }}
            >
              {wallet.network.chain}/{wallet.network.name}
              {wallet.assets.length === 0 && ' · not watched'}
              {!wallet.keyed && ' · watch only'}
            </Typography>
            {wallet.indexes.map((index) => {
              const progress = progressOf(index);
              const name = assetName(
                shell.listings,
                wallet.network.chain,
                wallet.network.name,
                index.asset,
              );
              return (
                <Typography key={index.asset} sx={{ fontSize: 11, color: progress.color }}>
                  {name}: {progress.text}
                </Typography>
              );
            })}
          </Box>
          <Tags
            shell={shell}
            wallet={wallet}
            tags={tags}
            adding={tagOpen}
            onClose={() => setTagOpen(false)}
            onTagged={onTagged}
          />
          <Box sx={{ flexGrow: 1 }} />
          <Typography
            variant="h3"
            component="p"
            title={claim?.text}
            sx={{ whiteSpace: 'nowrap', fontVariantNumeric: 'tabular-nums' }}
          >
            {balance ? format(balance.amount, { display }) : '—'}{' '}
            <Box component="span" sx={{ fontSize: '0.6em', color: 'text.secondary' }}>
              {unit}
            </Box>
          </Typography>
          <ButtonGroup size="small" variant="outlined" aria-label="Wallet actions">
            <Button onClick={() => setWatchOpen(true)}>
              {wallet.assets.length === 0 ? 'Watch' : 'Watch more'}
            </Button>
            <Button onClick={() => setTagOpen(true)}>Add tag</Button>
            {wallet.indexes.length > 0 && (
              <Button onClick={toggle}>{halt ? 'Resume' : 'Pause'}</Button>
            )}
          </ButtonGroup>
        </Stack>
        {claim && claim.color !== 'text.secondary' && (
          <Typography sx={{ fontSize: 12, color: claim.color, mt: 0.75, textAlign: 'right' }}>
            {claim.text}
          </Typography>
        )}
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
              <TableCell>Transaction</TableCell>
              <TableCell>Block</TableCell>
              <TableCell>Amount</TableCell>
              <TableCell>Fee</TableCell>
            </TableRow>
          </TableHead>
          <TableBody>
            {rows.length === 0 ? (
              <EmptyRow columns={4} message="No transactions yet." />
            ) : (
              rows.map((tx) => (
                <Row
                  key={tx.transaction_id}
                  wallet={wallet}
                  tx={tx}
                  decimals={decimals}
                  display={display}
                  unit={unit}
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

/** The wallet's tags, removable; `adding` opens the dialog that adds one. */
function Tags({
  shell,
  wallet,
  tags,
  adding,
  onClose,
  onTagged,
}: {
  shell: Shell;
  wallet: Account;
  tags: string[];
  adding: boolean;
  onClose: () => void;
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
        onClose();
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
    <>
      {tags.length > 0 && (
        <Stack direction="row" sx={{ flexWrap: 'wrap', gap: 0.75 }}>
          {tags.map((tag) => (
            <Chip key={tag} size="small" label={tag} onDelete={() => remove(tag)} />
          ))}
        </Stack>
      )}
      <Dialog open={adding} onClose={onClose} maxWidth="xs" fullWidth>
        <Box component="form" onSubmit={add}>
          <DialogTitle>Add tag</DialogTitle>
          <DialogContent>
            <TextField
              autoFocus
              fullWidth
              size="small"
              placeholder="treasury"
              value={text}
              onChange={(e) => setText(e.target.value)}
              helperText="Lowercase letters, digits and dashes; up to 64."
              slotProps={{
                htmlInput: {
                  'aria-label': 'Tag',
                  pattern: '[a-z0-9-]{1,64}',
                  spellCheck: false,
                },
              }}
              sx={{ mt: 1 }}
            />
          </DialogContent>
          <DialogActions>
            <Button variant="text" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" variant="contained">
              Add
            </Button>
          </DialogActions>
        </Box>
      </Dialog>
    </>
  );
}

function Row({
  wallet,
  tx,
  decimals,
  display,
  unit,
}: {
  wallet: Account;
  tx: Transaction;
  decimals: number;
  display: number | null;
  unit: string;
}) {
  const e = effect(tx, decimals);
  const { chain, name } = wallet.network;
  const block = ['networks', chain, name, 'blocks', String(tx.block.position)];
  return (
    <TableRow hover>
      <TableCell sx={{ ...figures, fontSize: 12, whiteSpace: 'nowrap' }}>
        <Tx value={tx.transaction_id} network={wallet.network} />
        {tx.outcome === 'reverted' && (
          <Box component="small" sx={{ color: 'error.main', ml: 1 }} title={tx.reason ?? undefined}>
            reverted
          </Box>
        )}
      </TableCell>
      <TableCell sx={figures}>
        <Link
          href={href(block)}
          underline="hover"
          onClick={(event) => {
            event.preventDefault();
            go(block);
          }}
        >
          {tx.block.position}
        </Link>
      </TableCell>
      <TableCell sx={figures}>
        <Money amount={e.net.toString()} decimals={decimals} display={display} ticker={unit} />
      </TableCell>
      <TableCell sx={figures}>
        {e.fee > 0n && (
          <Money amount={e.fee.toString()} decimals={decimals} display={display} ticker={unit} />
        )}
      </TableCell>
    </TableRow>
  );
}
