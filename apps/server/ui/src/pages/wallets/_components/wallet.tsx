import Decimal from 'decimal.js';
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
import ToggleButton from '@mui/material/ToggleButton';
import ToggleButtonGroup from '@mui/material/ToggleButtonGroup';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useTheme } from '@mui/material/styles';
import useMediaQuery from '@mui/material/useMediaQuery';
import { useCallback, useEffect, useState, type SubmitEvent } from 'react';
import { call, client, type Movement, type Wallet as Account } from '@/api/client';
import { Address } from '@/_components/address';
import { Currencies, Currency } from '@/_components/currency';
import { format } from '@/_components/format';
import { Money } from '@/_components/money';
import { LogsButton } from '@/_components/logs/button';
import { EmptyRow } from '@/_components/table';
import { Transaction as Tx } from '@/_components/transaction';
import { When } from '@/_components/when';
import type { Shell } from '@/layout';
import { assetName, assetOf, assetsOf, listingOf, networkOf, tickersOf } from '@/money';
import { go, href } from '@/route';
import { mono } from '@/theme';
import { Balances } from './balances';
import { Send } from './send';
import { WatchDialog } from './watch';
import { haltOf, progressOf } from './watching';

/** A leg's `kind`: 0 received, 1 spent, 2 burned, 3 minted, 4 fee, 5 reward, 6 adjustment. */
const KIND = ['received', 'spent', 'burned', 'minted', 'fee', 'reward', 'adjustment'];

/** The wallet's confirmed balance in `asset`, in atomic units, as `GET /v1/wallets/{id}` reports it. */
function amountOf(wallet: Account, asset: string): string {
  return wallet.indexes[asset]?.balance_confirmed ?? '0';
}

/** What it will hold once everything in flight lands: the newest block, with what this server sent and is not mined yet. */
function pendingOf(wallet: Account, asset: string): string {
  return wallet.indexes[asset]?.balance_pending ?? '0';
}

/** What a wallet's balance in `asset` says about the history behind it. */
function claimOf(wallet: Account, asset: string): { text: string; color: string } {
  if (amountOf(wallet, asset).startsWith('-')) {
    return {
      text: 'More left this address than ever reached it: credits are missing.',
      color: 'error.main',
    };
  }
  const index = wallet.indexes[asset];
  const through = index?.indexed_through;
  if (through == null) {
    return { text: 'Not indexed yet.', color: 'warning.main' };
  }
  const behind = index?.behind_tip ?? 0;
  if (behind > 0) {
    return {
      text: `Indexed through ${through} of ${through + behind}; later movements are not counted yet.`,
      color: 'warning.main',
    };
  }
  return { text: `Indexed through ${through}.`, color: 'text.secondary' };
}

/**
 * How confirmed the block at `position` is, as `n/N`: its confirmations so far (its own block
 * counts as one) of the `N` the network needs; `final` once its block is final.
 */
function confirmationsOf(
  position: number | null | undefined,
  index: Account['indexes'][string] | undefined,
): string {
  if (position == null || index?.indexed_through == null) return '—';
  const final = index.finality_at;
  if (final != null && position <= final) return 'final';
  const tip = index.indexed_through + (index.behind_tip ?? 0);
  const needed = final == null ? null : tip - final;
  const count = Math.max(tip - position + 1, 0);
  return needed == null ? String(count) : `${String(Math.min(count, needed))}/${String(needed)}`;
}

/** `amount` with its sign: `+` above zero, `-` below, none at zero. */
function signed(amount: Decimal, units: { decimals: number; display: number | null }): string {
  const text = format(amount.abs(), units);
  if (amount.isZero()) return text;
  return `${amount.isNegative() ? '-' : '+'}${text}`;
}

/** How often the wallet and its movements are read again. */
const REFRESH_MS = 10_000;

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
  const [fresh, setFresh] = useState<Account | null>(null);
  const [picked, setPicked] = useState(assetOf(wallet));
  const asset = picked in wallet.indexes ? picked : assetOf(wallet);
  const listing = listingOf(wallet, shell.listings, asset)?.metadata;
  const money = {
    decimals: listing?.decimals ?? 0,
    display: listing?.display_decimals ?? null,
    chain: wallet.chain,
    ticker: assetName(shell.listings, wallet.chain, wallet.network, asset),
  };
  const [legs, setLegs] = useState<Movement[]>([]);
  const [legsCursor, setLegsCursor] = useState<string | null>(null);
  const [watchOpen, setWatchOpen] = useState(false);
  const [tagOpen, setTagOpen] = useState(false);
  const [sendOpen, setSendOpen] = useState(false);

  const refresh = useCallback(() => {
    const id = { path: { id: wallet.id } };
    call(client.GET('/v1/wallets/{id}', { params: id }))
      .then(setFresh)
      .catch(shell.fail);
    const index = { path: { id: wallet.id, asset } };
    call(client.GET('/v1/wallets/{id}/indexes/{asset}/legs', { params: index }))
      .then((page) => {
        setLegs(page.legs);
        setLegsCursor(page.next_cursor ?? null);
      })
      .catch(shell.fail);
  }, [wallet, asset, shell.fail]);

  useEffect(() => {
    refresh();
    // Balances, movements and their confirmations move with every block.
    const timer = window.setInterval(refresh, REFRESH_MS);
    return () => window.clearInterval(timer);
  }, [refresh]);

  const olderLegs = () => {
    if (legsCursor === null) return;
    const params = {
      path: { id: wallet.id, asset },
      query: { cursor: legsCursor },
    };
    call(client.GET('/v1/wallets/{id}/indexes/{asset}/legs', { params }))
      .then((page) => {
        setLegs((current) => [...current, ...page.legs]);
        setLegsCursor(page.next_cursor ?? null);
      })
      .catch(shell.fail);
  };

  const theme = useTheme();
  const compact = !useMediaQuery(theme.breakpoints.up('sm'));

  /** `id`'s confirmed balance and what is pending on top of it. */
  const balanceOf = (id: string) => {
    const listed = listingOf(wallet, shell.listings, id)?.metadata;
    const units = { decimals: listed?.decimals ?? 0, display: listed?.display_decimals ?? null };
    const confirmed = fresh ? amountOf(fresh, id) : null;
    const pending = fresh ? pendingOf(fresh, id) : null;
    const change =
      pending === null || confirmed === null ? null : new Decimal(pending).minus(confirmed);
    return (
      // As tall as both lines, so every tab is one height; a lone amount sits in the middle.
      <Box
        sx={{
          display: 'flex',
          flexDirection: 'column',
          justifyContent: 'center',
          minHeight: 38,
          textAlign: 'left',
          fontVariantNumeric: 'tabular-nums',
        }}
      >
        <Typography sx={{ fontSize: 18, lineHeight: 1.2, color: 'text.primary' }}>
          {confirmed === null ? '—' : format(confirmed, units)}{' '}
          <Box component="span" sx={{ fontSize: 12, color: 'text.secondary' }}>
            {assetName(shell.listings, wallet.chain, wallet.network, id)}
          </Box>
        </Typography>
        {change !== null && !change.isZero() && (
          <Typography sx={{ fontSize: 11, color: 'text.secondary' }}>
            {signed(change, units)} pending
          </Typography>
        )}
      </Box>
    );
  };

  const claim = fresh ? claimOf(fresh, asset) : null;
  const halt = haltOf(wallet);
  const index = wallet.indexes[asset];
  const progress = index ? progressOf(index) : null;
  /** The picked asset's index as last read, with its balances. */
  const balances = (fresh ?? wallet).indexes[asset];

  /** Pauses every index of the wallet, or resumes them when one is stopped. */
  const toggle = () => {
    const index_state = halt ? 'indexing' : 'paused';
    Promise.all(
      Object.values(wallet.indexes).map(({ asset }) =>
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
            <Currencies chain={wallet.chain} tickers={tickersOf(wallet, shell.listings)} />
          </Box>
          <Box sx={{ minWidth: 0 }}>
            <Box sx={{ fontSize: 15 }}>
              <Address value={wallet.address} network={networkOf(wallet)} trim={false} />
            </Box>
            <Typography
              variant="overline"
              component="p"
              sx={{ whiteSpace: 'nowrap', lineHeight: 1.6 }}
            >
              {wallet.chain}/{wallet.network}
              {assetsOf(wallet).length === 0 && ' · not watched'}
              {!wallet.keyed && ' · watch only'}
            </Typography>
            {wallet.external_id && (
              <Typography sx={{ fontSize: 11, color: 'text.secondary' }}>
                External ID: {wallet.external_id}
              </Typography>
            )}
          </Box>
          <Box sx={{ flexGrow: 1 }} />
          <Stack spacing={1} sx={{ alignItems: 'flex-end' }}>
            <Stack direction="row" spacing={1} sx={{ alignItems: 'center' }}>
              <ButtonGroup
                size="small"
                variant="outlined"
                aria-label="Wallet actions"
                sx={{ flexWrap: 'wrap' }}
              >
                {wallet.keyed && assetsOf(wallet).length > 0 && (
                  <Button onClick={() => setSendOpen(true)}>Send</Button>
                )}
                <Button onClick={() => setWatchOpen(true)}>
                  {assetsOf(wallet).length === 0 ? 'Watch' : 'Watch more'}
                </Button>
                <Button onClick={() => setTagOpen(true)}>Add tag</Button>
                {assetsOf(wallet).length > 0 && (
                  <Button onClick={toggle}>{halt ? 'Resume' : 'Pause'}</Button>
                )}
              </ButtonGroup>
              <LogsButton filter={{ wallet_id: wallet.id }} />
            </Stack>
            <Tags
              shell={shell}
              wallet={wallet}
              tags={tags}
              adding={tagOpen}
              onClose={() => setTagOpen(false)}
              onTagged={onTagged}
            />
          </Stack>
        </Stack>
      </Box>

      {wallet.keyed && (
        <Send
          shell={shell}
          wallet={wallet}
          asset={asset}
          open={sendOpen}
          onClose={() => setSendOpen(false)}
          onSent={refresh}
        />
      )}

      {assetsOf(wallet).length > 0 && (
        // Each asset with its balance; the one picked shows its progress and movements. Native
        // first. On a phone the buttons are its icons, and the picked one's balance shows below.
        <ToggleButtonGroup
          exclusive
          value={asset}
          onChange={(_, value: string | null) => value && setPicked(value)}
          aria-label="Asset"
          sx={{ mt: 3, flexWrap: 'wrap', maxWidth: '100%' }}
        >
          {assetsOf(wallet).map((id) => (
            <ToggleButton
              key={id}
              value={id}
              aria-label={assetName(shell.listings, wallet.chain, wallet.network, id)}
              sx={{
                px: compact ? 1.5 : 2.5,
                py: 1,
                minWidth: compact ? 0 : 220,
                justifyContent: 'flex-start',
                gap: 1.5,
                textTransform: 'none',
                alignItems: 'center',
              }}
            >
              <Box sx={{ display: 'flex', fontSize: 22 }}>
                <Currency
                  chain={wallet.chain}
                  ticker={assetName(shell.listings, wallet.chain, wallet.network, id)}
                  label={false}
                />
              </Box>
              {!compact && balanceOf(id)}
            </ToggleButton>
          ))}
        </ToggleButtonGroup>
      )}
      {balances && <Balances index={balances} units={money} />}
      {claim && claim.color !== 'text.secondary' && (
        <Typography sx={{ fontSize: 12, color: claim.color, mt: 0.5 }}>{claim.text}</Typography>
      )}
      {progress && (
        <Typography sx={{ fontSize: 11, color: progress.color, mt: 1.5, mb: 1 }}>
          {progress.text}
        </Typography>
      )}
      <TableContainer>
        <Table>
          <TableHead>
            <TableRow>
              <TableCell>Kind</TableCell>
              <TableCell>Amount</TableCell>
              <TableCell>Balance after</TableCell>
              <TableCell>Time</TableCell>
              <TableCell>Block</TableCell>
              <TableCell>Confirmations</TableCell>
              <TableCell>Transaction</TableCell>
            </TableRow>
          </TableHead>
          <TableBody>
            {legs.length === 0 ? (
              <EmptyRow columns={7} message="No movements yet." />
            ) : (
              legs.map((leg) => {
                const out = [1, 2, 4].includes(leg.kind) ? '-' : '';
                const block = [
                  'networks',
                  wallet.chain,
                  wallet.network,
                  'blocks',
                  `${leg.position}`,
                ];
                return (
                  <TableRow key={leg.id} hover>
                    <TableCell>{KIND[leg.kind]}</TableCell>
                    <TableCell sx={figures}>
                      <Money amount={`${out}${leg.amount}`} {...money} />
                      {leg.amount_usd != null && (
                        <Box component="small" sx={{ color: 'text.secondary', ml: 1 }}>
                          ≈ ${format(leg.amount_usd, { display: 2 })}
                        </Box>
                      )}
                    </TableCell>
                    <TableCell sx={figures}>
                      <Money amount={leg.total ?? '0'} {...money} />
                    </TableCell>
                    <TableCell sx={{ whiteSpace: 'nowrap' }}>
                      {leg.timestamp == null ? '—' : <When at={leg.timestamp * 1000} />}
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
                        {leg.position}
                      </Link>
                    </TableCell>
                    <TableCell sx={{ ...figures, whiteSpace: 'nowrap' }}>
                      {confirmationsOf(leg.position, (fresh ?? wallet).indexes[asset])}
                    </TableCell>
                    <TableCell sx={{ ...figures, fontSize: 12, whiteSpace: 'nowrap' }}>
                      {leg.txid && <Tx value={leg.txid} network={networkOf(wallet)} />}
                      {leg.state === 2 && (
                        <Box component="small" sx={{ color: 'error.main', ml: 1 }}>
                          reverted
                        </Box>
                      )}
                    </TableCell>
                  </TableRow>
                );
              })
            )}
          </TableBody>
        </Table>
      </TableContainer>
      {legsCursor !== null && (
        <Button onClick={olderLegs} sx={{ mt: 1.5 }}>
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
          shell.notify(`Watching ${assetsOf(watched).join(', ')}.`);
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
