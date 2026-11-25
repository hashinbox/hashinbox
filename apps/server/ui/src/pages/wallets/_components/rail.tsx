import Add from '@mui/icons-material/Add';
import Autocomplete, { createFilterOptions } from '@mui/material/Autocomplete';
import FilterList from '@mui/icons-material/FilterList';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Chip from '@mui/material/Chip';
import Collapse from '@mui/material/Collapse';
import Dialog from '@mui/material/Dialog';
import DialogActions from '@mui/material/DialogActions';
import DialogContent from '@mui/material/DialogContent';
import DialogContentText from '@mui/material/DialogContentText';
import DialogTitle from '@mui/material/DialogTitle';
import IconButton from '@mui/material/IconButton';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Tooltip from '@mui/material/Tooltip';
import Typography from '@mui/material/Typography';
import ToggleButton from '@mui/material/ToggleButton';
import ToggleButtonGroup from '@mui/material/ToggleButtonGroup';
import Decimal from 'decimal.js';
import { useState, type SubmitEvent } from 'react';
import { call, client, type Wallet } from '@/api/client';
import { Currencies } from '@/_components/currency';
import type { Shell } from '@/layout';
import { assetOf, decimalsOf, networkChoices, tickersOf, unitOf } from '@/money';
import { mono } from '@/theme';
import { WatchFields } from './watch';
import { WATCH_NOTHING, haltOf, watch, type Watching } from './watching';
import { NetworkSelect } from '@/_components/network-select';
import { Side, SideItem } from '@/_components/side';

interface Props {
  shell: Shell;
  wallets: Wallet[];
  selected: string | null;
  onSelect: (id: string) => void;
  onAdded: (wallet: Wallet) => void;
  /** Tags by wallet id. */
  tags: Map<string, string[]>;
}

/** One tag of the filter; `address` is free text matched against address and id. */
interface Filter {
  group: 'network' | 'currency' | 'kind' | 'tag' | 'balance' | 'address';
  value: string;
  label: string;
}
const GROUPS: Filter['group'][] = ['network', 'currency', 'kind', 'tag', 'balance', 'address'];

/** A typed balance comparison, e.g. `> 100` or `<=0.5`. */
const COMPARISON = /^(>=|<=|>|<|=)\s*(-?\d+(?:\.\d+)?)$/;
const listed = createFilterOptions<Filter>();

/** Whether exact display amount `amount` passes balance filter `test`. */
function passes(amount: Decimal, test: string): boolean {
  if (test === 'empty') return amount.isZero();
  if (test === 'not empty') return !amount.isZero();
  const [, op, bound = '0'] = COMPARISON.exec(test) ?? [];
  const order = amount.cmp(bound);
  if (op === '>') return order > 0;
  if (op === '>=') return order >= 0;
  if (op === '<') return order < 0;
  if (op === '<=') return order <= 0;
  return order === 0;
}

/** The address with its middle elided, so both ends stay checkable. */
function middle(text: string): string {
  return text.length > 22 ? `${text.slice(0, 10)}…${text.slice(-8)}` : text;
}

export function Rail({ shell, wallets, selected, onSelect, onAdded, tags }: Props) {
  const [creating, setCreating] = useState(false);
  const [filtering, setFiltering] = useState(false);
  const [filters, setFilters] = useState<Filter[]>([]);

  const matches = (w: Wallet, f: Filter): boolean => {
    const tagged = tags.get(w.id) ?? [];
    switch (f.group) {
      case 'network':
        return `${w.chain}/${w.network}` === f.value;
      case 'currency':
        return unitOf(w, shell.listings) === f.value;
      case 'kind':
        return w.keyed === (f.value === 'keyed');
      case 'tag':
        return f.value === '' ? tagged.length === 0 : tagged.includes(f.value);
      case 'balance':
        // In the wallet's currency (atomic units over its decimals); none
        // indexed reads as zero.
        return passes(
          new Decimal(w.indexes[assetOf(w)]?.balance_confirmed ?? '0').div(
            new Decimal(10).pow(decimalsOf(w, shell.listings)),
          ),
          f.value,
        );
      case 'address': {
        const needle = f.value.toLowerCase();
        return w.address.toLowerCase().includes(needle) || w.id.toLowerCase().includes(needle);
      }
    }
  };
  // Groups combine with AND; values within a group with OR.
  const shown = wallets.filter((w) =>
    GROUPS.every((g) => {
      const of = filters.filter((f) => f.group === g);
      return of.length === 0 || of.some((f) => matches(w, f));
    }),
  );
  const sorted = (values: string[]) => [...new Set(values)].sort();
  const options: Filter[] = [
    ...sorted(wallets.map((w) => `${w.chain}/${w.network}`)).map((n) => ({
      group: 'network' as const,
      value: n,
      label: n,
    })),
    // By currency (ETH, USDC…), whatever network it is on.
    ...sorted(wallets.map((w) => unitOf(w, shell.listings))).map((a) => ({
      group: 'currency' as const,
      value: a,
      label: a,
    })),
    { group: 'kind', value: 'keyed', label: 'keyed' },
    { group: 'kind', value: 'watch', label: 'watch only' },
    ...sorted([...tags.values()].flat()).map((t) => ({
      group: 'tag' as const,
      value: t,
      label: `#${t}`,
    })),
    { group: 'tag', value: '', label: 'no tag' },
    { group: 'balance', value: 'empty', label: 'empty' },
    { group: 'balance', value: 'not empty', label: 'not empty' },
  ];

  return (
    <>
      <Side
        title="Wallets"
        count={
          shown.length === wallets.length
            ? String(wallets.length)
            : `${String(shown.length)}/${String(wallets.length)}`
        }
        actions={
          <>
            <Tooltip title="Filter">
              <IconButton
                size="small"
                aria-label="Filter wallets"
                aria-pressed={filtering}
                onClick={() => setFiltering(!filtering)}
                sx={{ color: filtering ? 'text.primary' : 'text.secondary' }}
              >
                <FilterList fontSize="small" />
              </IconButton>
            </Tooltip>
            <Tooltip title="New wallet">
              <IconButton
                size="small"
                aria-label="New wallet"
                onClick={(e) => {
                  e.currentTarget.blur();
                  setCreating(true);
                }}
              >
                <Add fontSize="small" />
              </IconButton>
            </Tooltip>
          </>
        }
        tools={
          <Collapse in={filtering}>
            <Stack spacing={1.25} sx={{ pb: 2 }}>
              <Autocomplete
                multiple
                freeSolo
                autoHighlight
                size="small"
                options={options}
                value={filters}
                groupBy={(o) => o.group}
                getOptionLabel={(o) => (typeof o === 'string' ? o : o.label)}
                isOptionEqualToValue={(o, v) =>
                  typeof v !== 'string' && o.group === v.group && o.value === v.value
                }
                filterOptions={(all, state) => {
                  // `> 100` offers itself as a balance comparison.
                  const [, op, bound] = COMPARISON.exec(state.inputValue.trim()) ?? [];
                  const compared: Filter[] =
                    op && bound
                      ? [{ group: 'balance', value: `${op} ${bound}`, label: `${op} ${bound}` }]
                      : [];
                  return [...compared, ...listed(all, state)];
                }}
                onChange={(_, next) => {
                  // Free text that matched no option filters by address or id.
                  const picked = next
                    .map((t): Filter =>
                      typeof t === 'string'
                        ? { group: 'address', value: t.trim(), label: t.trim() }
                        : t,
                    )
                    .filter((t) => t.group !== 'address' || t.value !== '');
                  const first = (t: Filter, i: number) =>
                    picked.findIndex((u) => u.group === t.group && u.value === t.value) === i;
                  setFilters(picked.filter(first));
                }}
                renderValue={(_, getItemProps) =>
                  filters.map((t, index) => (
                    <Chip
                      {...getItemProps({ index })}
                      key={`${t.group}:${t.value}`}
                      size="small"
                      label={
                        <>
                          <Box component="span" sx={{ color: 'text.secondary', mr: 0.5 }}>
                            {t.group}
                          </Box>
                          {t.label}
                        </>
                      }
                      sx={{ borderRadius: '6px', bgcolor: 'grey.100', fontSize: 12 }}
                    />
                  ))
                }
                renderInput={(params) => (
                  <TextField
                    {...params}
                    placeholder={filters.length === 0 ? 'Filter by tag, balance, address…' : ''}
                    slotProps={{
                      ...params.slotProps,
                      input: {
                        ...params.slotProps.input,
                        startAdornment: (
                          <>
                            <FilterList
                              fontSize="small"
                              aria-hidden
                              sx={{ color: 'text.secondary', ml: 0.5, mr: 0.25 }}
                            />
                            {params.slotProps.input.startAdornment}
                          </>
                        ),
                      },
                      htmlInput: {
                        ...params.slotProps.htmlInput,
                        'aria-label': 'Filter wallets',
                        spellCheck: false,
                      },
                    }}
                    sx={{ '& .MuiOutlinedInput-root': { gap: 0.5, fontSize: 13 } }}
                  />
                )}
              />
            </Stack>
          </Collapse>
        }
        empty={
          shown.length === 0 && (wallets.length === 0 ? 'No wallets yet.' : 'Nothing matches.')
        }
      >
        {shown.map((w) => (
          <SideItem
            key={w.id}
            icon={<Currencies chain={w.chain} tickers={tickersOf(w, shell.listings)} />}
            name={middle(w.address)}
            dot={dotOf(w)}
            selected={w.id === selected}
            onClick={() => onSelect(w.id)}
            detail={`${w.chain}/${w.network}${(tags.get(w.id) ?? []).map((t) => ` · #${t}`).join('')}`}
          />
        ))}
      </Side>

      <CreateDialog
        shell={shell}
        open={creating}
        onClose={() => setCreating(false)}
        onDone={(wallet) => {
          setCreating(false);
          onAdded(wallet);
          shell.notify('Wallet added.');
        }}
      />
    </>
  );
}

interface DialogProps {
  shell: Shell;
  open: boolean;
  onClose: () => void;
  onDone: (wallet: Wallet) => void;
}

/** The networks the configured assets live in, as `chain/network`. */
function networksOf(shell: Shell): string[] {
  return [...new Set(shell.listings.map((l) => `${l.network.chain}/${l.network.name}`))];
}

/** How the wallet of step 1 is made. */
type Kind = 'generate' | 'secret' | 'address';

const ABOUT: Record<Kind, string> = {
  generate: 'Generates a key and keeps it on this server.',
  secret: 'Imports a raw private key (Ethereum hex, Solana Base58, Bitcoin WIF) and keeps it here.',
  address: 'Watches an existing address; its key is not kept here, so it cannot send.',
};

/** Adds a wallet in two steps: (1) the wallet, (2) what it is indexed in, from when. */
function CreateDialog({ shell, open, onClose, onDone }: DialogProps) {
  const [kind, setKind] = useState<Kind>('generate');
  const [network, setNetwork] = useState('');
  const [text, setText] = useState('');
  const [tags, setTags] = useState<string[]>([]);
  const [externalId, setExternalId] = useState('');
  // The wallet step 1 made; step 2 indexes it.
  const [made, setMade] = useState<Wallet | null>(null);
  const [watching, setWatching] = useState<Watching>({ ...WATCH_NOTHING, assets: ['native'] });
  const [busy, setBusy] = useState(false);
  const chosen = network || (networksOf(shell)[0] ?? '');
  const [chain = '', name = ''] = chosen.split('/');
  const offered = shell.listings.filter(
    (l) => l.network.chain === chain && l.network.name === name,
  );
  const ready = made ? watching.assets.length > 0 : kind === 'generate' || text.trim() !== '';

  const finish = (wallet: Wallet) => {
    setText('');
    setTags([]);
    setExternalId('');
    setMade(null);
    setWatching({ ...WATCH_NOTHING, assets: ['native'] });
    onDone(wallet);
  };

  /** Step 1: makes the wallet and tags it. */
  const make = async () => {
    const key = text.trim();
    // With neither a key nor an address, the server generates the key.
    const body = {
      chain,
      network: name,
      ...(kind === 'secret' && { secret: key }),
      ...(kind === 'address' && { address: key }),
      ...(externalId.trim() !== '' && { external_id: externalId.trim() }),
    };
    const wallet = await call(client.POST('/v1/wallets', { body }));
    for (const tag of tags) {
      const params = { path: { id: wallet.id, tag } };
      await call(client.PUT('/v1/wallets/{id}/tags/{tag}', { params }));
    }
    setText('');
    setMade(wallet);
  };

  /** Step 2: indexes it. */
  const index = async (wallet: Wallet) => {
    finish(
      await watch(wallet.id, kind === 'generate' ? { ...watching, start: 'fresh' } : watching),
    );
  };

  return (
    <Dialog open={open} onClose={made ? () => finish(made) : onClose} fullWidth maxWidth="xs">
      <Box
        component="form"
        onSubmit={(event: SubmitEvent<HTMLFormElement>) => {
          event.preventDefault();
          setBusy(true);
          (made ? index(made) : make()).catch(shell.fail).finally(() => setBusy(false));
        }}
      >
        <DialogTitle variant="h3">
          Add wallet{' '}
          <Typography component="span" color="text.secondary" sx={{ fontSize: 13 }}>
            {made ? '2/2 · Indexing' : '1/2 · Wallet'}
          </Typography>
        </DialogTitle>
        <DialogContent>
          {made ? (
            <Stack spacing={2} sx={{ mt: 1 }}>
              <DialogContentText sx={{ fontSize: 13 }}>
                <Box component="span" sx={{ fontFamily: mono }}>
                  {middle(made.address)}
                </Box>{' '}
                is kept. Index it in its assets to see balances and history.
              </DialogContentText>
              <WatchFields
                offered={offered}
                value={watching}
                onChange={setWatching}
                freshOnly={kind === 'generate'}
              />
            </Stack>
          ) : (
            <Stack spacing={2} sx={{ mt: 1 }}>
              <NetworkSelect
                value={chosen}
                networks={networkChoices(shell.listings)}
                onChange={(value) => {
                  setNetwork(value);
                  setWatching({ ...watching, assets: ['native'] });
                }}
                required
              />
              <ToggleButtonGroup
                exclusive
                fullWidth
                size="small"
                value={kind}
                onChange={(_, next: Kind | null) => {
                  if (!next) return;
                  setKind(next);
                  setText('');
                }}
              >
                <ToggleButton value="generate">Generate</ToggleButton>
                <ToggleButton value="secret">Private key</ToggleButton>
                <ToggleButton value="address">Address</ToggleButton>
              </ToggleButtonGroup>
              <DialogContentText sx={{ fontSize: 13 }}>{ABOUT[kind]}</DialogContentText>
              {kind !== 'generate' && (
                <TextField
                  label={kind === 'secret' ? 'Private key' : 'Address'}
                  type={kind === 'secret' ? 'password' : 'text'}
                  value={text}
                  onChange={(e) => setText(e.target.value)}
                  autoComplete="off"
                  slotProps={{ htmlInput: { spellCheck: false } }}
                  required
                />
              )}
              <Autocomplete
                multiple
                freeSolo
                options={[]}
                value={tags}
                onChange={(_, next) =>
                  setTags(next.map((t) => t.trim().toLowerCase()).filter((t) => t !== ''))
                }
                renderInput={(params) => (
                  <TextField {...params} label="Tags (optional)" placeholder="deposit" />
                )}
              />
              <TextField
                label="External ID (optional)"
                placeholder="Your customer or account ID"
                value={externalId}
                onChange={(e) => setExternalId(e.target.value)}
                autoComplete="off"
                slotProps={{ htmlInput: { maxLength: 128, spellCheck: false } }}
              />
            </Stack>
          )}
        </DialogContent>
        <DialogActions>
          {made ? (
            <Button color="inherit" onClick={() => finish(made)}>
              Later
            </Button>
          ) : (
            <Button color="inherit" onClick={onClose}>
              Cancel
            </Button>
          )}
          <Button type="submit" variant="contained" loading={busy} disabled={!ready}>
            {made ? 'Index' : 'Next'}
          </Button>
        </DialogActions>
      </Box>
    </Dialog>
  );
}

/** A wallet's state as its dot: indexing stopped (paused, no data), not watched, or watching. */
function dotOf(wallet: Wallet): { color: string; label: string } {
  const halt = haltOf(wallet);
  if (halt) return { color: halt.color, label: halt.text };
  if (Object.keys(wallet.indexes).length === 0)
    return { color: 'text.disabled', label: 'not watched' };
  return { color: 'success.main', label: 'watching' };
}
