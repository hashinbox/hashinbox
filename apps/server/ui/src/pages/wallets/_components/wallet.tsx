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
import Tab from '@mui/material/Tab';
import Table from '@mui/material/Table';
import TableBody from '@mui/material/TableBody';
import TableCell from '@mui/material/TableCell';
import TableContainer from '@mui/material/TableContainer';
import TableHead from '@mui/material/TableHead';
import TableRow from '@mui/material/TableRow';
import Tabs from '@mui/material/Tabs';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useCallback, useEffect, useState, type SubmitEvent } from 'react';
import { call, client, type Movement, type Wallet as Account } from '@/api/client';
import { Address } from '@/_components/address';
import { Currency } from '@/_components/currency';
import { format } from '@/_components/format';
import { Money } from '@/_components/money';
import { EmptyRow } from '@/_components/table';
import { Transaction as Tx } from '@/_components/transaction';
import type { Shell } from '@/layout';
import {
  assetName,
  assetOf,
  assetsOf,
  decimalsOf,
  displayOf,
  listingOf,
  networkOf,
  unitOf,
} from '@/money';
import { go, href } from '@/route';
import { mono } from '@/theme';
import { Send } from './send';
import { WatchDialog } from './watch';
import { haltOf, progressOf } from './watching';

/** A leg's `kind`: 0 received, 1 spent, 2 burned, 3 minted, 4 fee, 5 reward. */
const KIND = ['received', 'spent', 'burned', 'minted', 'fee', 'reward'];

/** The wallet's confirmed balance in its asset, in atomic units, as `GET /v1/wallets/{id}` reports it. */
function amountOf(wallet: Account): string {
  return wallet.indexes[assetOf(wallet)]?.balance ?? '0';
}

/** Its balance at the tip, unconfirmed blocks included. */
function pendingOf(wallet: Account): string {
  return wallet.indexes[assetOf(wallet)]?.balance_pending ?? '0';
}

/** What a wallet's balance says about the history behind it. */
function claimOf(wallet: Account): { text: string; color: string } {
  if (amountOf(wallet).startsWith('-')) {
    return {
      text: 'More left this address than ever reached it: credits are missing.',
      color: 'error.main',
    };
  }
  const index = wallet.indexes[assetOf(wallet)];
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
  const [fresh, setFresh] = useState<Account | null>(null);
  const [picked, setPicked] = useState(assetOf(wallet));
  const asset = picked in wallet.indexes ? picked : assetOf(wallet);
  const listing = listingOf(wallet, shell.listings, asset)?.metadata;
  const money = {
    decimals: listing?.decimals ?? 0,
    display: listing?.display_decimals ?? null,
    ticker: assetName(shell.listings, wallet.chain, wallet.network, asset),
  };
  const [legs, setLegs] = useState<Movement[]>([]);
  const [legsCursor, setLegsCursor] = useState<string | null>(null);
  const [watchOpen, setWatchOpen] = useState(false);
  const [tagOpen, setTagOpen] = useState(false);

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

  useEffect(refresh, [refresh]);

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

  const claim = fresh ? claimOf(fresh) : null;
  const halt = haltOf(wallet);

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
            <Currency ticker={unit} label={false} />
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
            {Object.values(wallet.indexes).map((index) => {
              const progress = progressOf(index);
              const name = assetName(shell.listings, wallet.chain, wallet.network, index.asset);
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
            {fresh ? format(amountOf(fresh), { decimals, display }) : '—'}{' '}
            <Box component="span" sx={{ fontSize: '0.6em', color: 'text.secondary' }}>
              {unit}
            </Box>
            {fresh && pendingOf(fresh) !== amountOf(fresh) && (
              <Box component="span" sx={{ fontSize: '0.4em', color: 'text.secondary', ml: 1 }}>
                {format(pendingOf(fresh), { decimals, display })} pending
              </Box>
            )}
          </Typography>
          <ButtonGroup size="small" variant="outlined" aria-label="Wallet actions">
            <Button onClick={() => setWatchOpen(true)}>
              {assetsOf(wallet).length === 0 ? 'Watch' : 'Watch more'}
            </Button>
            <Button onClick={() => setTagOpen(true)}>Add tag</Button>
            {assetsOf(wallet).length > 0 && (
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
        Movements
      </Typography>
      {assetsOf(wallet).length > 1 && (
        <Tabs value={asset} onChange={(_, value: string) => setPicked(value)}>
          {assetsOf(wallet).map((id) => (
            <Tab
              key={id}
              value={id}
              label={assetName(shell.listings, wallet.chain, wallet.network, id)}
            />
          ))}
        </Tabs>
      )}
      <TableContainer>
        <Table>
          <TableHead>
            <TableRow>
              <TableCell>Kind</TableCell>
              <TableCell>Amount</TableCell>
              <TableCell>Balance after</TableCell>
              <TableCell>Block</TableCell>
              <TableCell>Transaction</TableCell>
            </TableRow>
          </TableHead>
          <TableBody>
            {legs.length === 0 ? (
              <EmptyRow columns={5} message="No movements yet." />
            ) : (
              legs.map((leg) => {
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
                      <Money
                        amount={[1, 2, 4].includes(leg.kind) ? `-${leg.amount}` : leg.amount}
                        {...money}
                      />
                    </TableCell>
                    <TableCell sx={figures}>
                      <Money amount={leg.total ?? '0'} {...money} />
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
