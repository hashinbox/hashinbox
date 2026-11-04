import Autocomplete from '@mui/material/Autocomplete';
import Box from '@mui/material/Box';
import Chip from '@mui/material/Chip';
import TextField from '@mui/material/TextField';
import { Currency } from '@/_components/currency';
import { mono } from '@/theme';
import { Block, Choice, LogicEditor, Row, Word } from './logic';
import {
  AGGREGATES,
  OPERATORS,
  duration,
  indexes,
  paired,
  rekindAmount,
  sides,
  variant,
  type Aggregate,
  type Amount,
  type Choices,
  type Compare,
  type FilterLeaf,
  type GateLeaf,
  type Logic,
  type Operator,
  type Pick,
  type Selector,
  type Target,
} from './forms';

/** An asset of the network. */
function AssetChoice({
  value,
  choices,
  onChange,
}: {
  value: string;
  choices: Choices;
  onChange: (asset: string) => void;
}) {
  return (
    <Choice
      label="Asset"
      value={value}
      options={choices.assets.map((a) => ({ value: a, label: choices.name(a) }))}
      onChange={onChange}
    />
  );
}

const PICKS = [
  { value: 'tag', label: 'tagged' },
  { value: 'asset', label: 'in asset' },
  { value: 'ids', label: 'picked' },
];

/** Wallets by tag, asset or hand, under all / any / not, to any depth. */
export function SelectorWords({
  value,
  choices,
  onChange,
}: {
  value: Selector;
  choices: Choices;
  onChange: (value: Selector) => void;
}) {
  const options = indexes(choices);
  const make = (kind: string): Pick =>
    kind === 'ids'
      ? { ids: [] }
      : kind === 'asset'
        ? { asset: choices.assets[0] ?? 'native' }
        : firstTag(choices);
  return (
    <LogicEditor<Pick>
      label="Which wallets"
      value={value}
      onChange={onChange}
      leaves={PICKS}
      kindOf={(p) => ('untagged' in p ? 'tag' : variant(p))}
      make={make}
      groups={{ all: 'matching all of', any: 'matching any of', not: 'not matching' }}
      empty={{ all: null, any: null }}
      leaf={(pick, set) =>
        'tag' in pick || 'untagged' in pick ? (
          <Autocomplete
            key="tag"
            options={[NO_TAG, ...choices.tags]}
            getOptionLabel={(t) => (t === NO_TAG ? 'no tag' : t)}
            value={'untagged' in pick || pick.tag === '' ? NO_TAG : pick.tag}
            onChange={(_, tag) => set(tag === NO_TAG ? { untagged: {} } : { tag })}
            disableClearable
            renderInput={(params) => (
              <TextField {...params} placeholder="pick a tag" aria-label="Tag" />
            )}
            sx={{ width: 200 }}
          />
        ) : 'asset' in pick ? (
          <AssetChoice value={pick.asset} choices={choices} onChange={(asset) => set({ asset })} />
        ) : (
          <Autocomplete
            key="ids"
            multiple
            options={options.map((o) => o.id)}
            getOptionLabel={(id) => options.find((o) => o.id === id)?.label ?? id}
            value={pick.ids}
            onChange={(_, ids) => set({ ids })}
            renderOption={(props, id) => {
              const { key, ...rest } = props;
              const o = options.find((x) => x.id === id);
              return (
                <Box component="li" key={key} {...rest} sx={{ gap: 1, whiteSpace: 'nowrap' }}>
                  {o ? <WalletLine wallet={o} /> : id}
                </Box>
              );
            }}
            renderValue={(ids, getItemProps) =>
              ids.map((id, index) => {
                const o = options.find((x) => x.id === id);
                const { key, ...rest } = getItemProps({ index });
                return (
                  <Chip
                    key={key}
                    {...rest}
                    size="small"
                    label={o ? <WalletLine wallet={o} short /> : id}
                    sx={{ borderRadius: '6px' }}
                  />
                );
              })
            }
            slotProps={{ popper: { placement: 'bottom-start', style: { minWidth: 380 } } }}
            renderInput={(params) => (
              <TextField
                {...params}
                placeholder={pick.ids.length === 0 ? 'pick wallets' : ''}
                aria-label="Wallets"
              />
            )}
            sx={{ minWidth: 260, flexGrow: 1 }}
          />
        )
      }
    />
  );
}

/** "This wallet" (as `choices.self` calls it), in another asset maybe, or chosen wallets. */
export function TargetWords({
  value,
  choices,
  onChange,
  thisOnly = false,
}: {
  value: Target;
  choices: Choices;
  onChange: (value: Target) => void;
  /** Only the wallet in context: a filter's left side must read it. */
  thisOnly?: boolean;
}) {
  const options = [
    { value: 'this', label: choices.self },
    ...choices.assets.map((a) => ({
      value: `this:${a}`,
      label: `${choices.self} in ${choices.name(a)}`,
    })),
    // Still listed when a stored filter has it, so it can be switched away.
    ...(thisOnly && !('select' in value) ? [] : [{ value: 'select', label: 'wallets' }]),
  ];
  const current =
    'select' in value
      ? 'select'
      : value.this.asset === undefined
        ? 'this'
        : `this:${value.this.asset}`;
  return (
    <>
      <Choice
        label="Which wallets"
        value={current}
        options={options}
        onChange={(k) => {
          if (k === current) return;
          if (k === 'select') onChange({ select: firstTag(choices) });
          else if (k === 'this') onChange({ this: {} });
          else onChange({ this: { asset: k.slice('this:'.length) } });
        }}
      />
      {'select' in value && (
        <SelectorWords
          value={value.select}
          choices={choices}
          onChange={(select) => onChange({ select })}
        />
      )}
    </>
  );
}

const AMOUNTS = [
  { value: 'value', label: 'fixed amount' },
  { value: 'balance', label: 'balance of' },
  { value: 'fee', label: 'fee to send all of' },
  { value: 'add', label: 'sum of' },
  { value: 'sub', label: 'difference of' },
  { value: 'min', label: 'lower of' },
  { value: 'max', label: 'higher of' },
];

/** The word joining the two sides of each two-sided amount. */
const JOIN: Record<string, string> = { add: 'plus', sub: 'minus', min: 'and', max: 'and' };

/** A decimal as the server parses it. */
const decimal = (text: string) => /^-?\d+(\.\d+)?$/.test(text.trim());

/** An amount as words: a fixed amount, a balance, a fee, or two combined. */
export function AmountWords({
  value,
  choices,
  onChange,
}: {
  value: Amount;
  choices: Choices;
  onChange: (value: Amount) => void;
}) {
  const kind = variant(value);
  const pair = sides(value);
  const of = 'balance' in value ? value.balance.of : 'fee' in value ? value.fee.of : undefined;
  const setPair = (next: [Amount, Amount]) => onChange(paired(kind, next));
  return (
    <>
      <Choice
        label="Amount"
        value={kind}
        options={AMOUNTS}
        onChange={(k) => onChange(rekindAmount(value, k))}
      />
      {'value' in value && (
        <TextField
          fullWidth={false}
          placeholder="0"
          value={value.value}
          error={!decimal(value.value)}
          onChange={(e) => onChange({ value: e.target.value })}
          slotProps={{
            htmlInput: { inputMode: 'decimal', spellCheck: false, 'aria-label': 'Amount' },
          }}
          sx={{ width: 120 }}
        />
      )}
      {('balance' in value || 'fee' in value) && (
        <TargetWords
          value={of ?? { this: {} }}
          choices={choices}
          onChange={(target) =>
            onChange('balance' in value ? { balance: { of: target } } : { fee: { of: target } })
          }
        />
      )}
      {pair && (
        <Block>
          <Row>
            <AmountWords
              value={pair[0]}
              choices={choices}
              onChange={(a) => setPair([a, pair[1]])}
            />
          </Row>
          <Row>
            <Word>{JOIN[kind]}</Word>
            <AmountWords
              value={pair[1]}
              choices={choices}
              onChange={(b) => setPair([pair[0], b])}
            />
          </Row>
        </Block>
      )}
    </>
  );
}

/** The words after a compare's aggregate: of which wallets, is how, than what. */
function CompareWords({
  value,
  choices,
  onChange,
  perWallet = false,
}: {
  value: Compare;
  choices: Choices;
  onChange: (value: Compare) => void;
  /** In a filter: it tests one wallet, so its left side is that wallet. */
  perWallet?: boolean;
}) {
  return (
    <>
      <TargetWords
        value={value.of}
        choices={choices}
        onChange={(of) => onChange({ ...value, of })}
        thisOnly={perWallet}
      />
      <Word>is</Word>
      <Choice
        label="Comparison"
        value={value.operator}
        options={OPERATORS.map((o) => ({
          value: o.value,
          label: o.label,
          menu: `${o.symbol}  ${o.label}`,
        }))}
        onChange={(operator) => onChange({ ...value, operator: operator as Operator })}
      />
      <AmountWords
        value={value.value}
        choices={choices}
        onChange={(amount) => onChange({ ...value, value: amount })}
      />
    </>
  );
}

const COMPARE: Compare = {
  of: { this: {} },
  aggregate: 'sum',
  operator: 'above',
  value: { value: '0' },
};

const COMPARES = AGGREGATES.map((a) => ({ value: `compare:${a.value}`, label: `the ${a.label}` }));

/** A compare leaf turned to `kind` (`compare:<aggregate>`), keeping the rest of `from`. */
const compareOf = (kind: string, from?: Compare): Compare => ({
  ...(from ?? COMPARE),
  aggregate: kind.slice('compare:'.length) as Aggregate,
});

const GROUPS = { all: 'all of these hold', any: 'any of these holds', not: 'not' };

/** Which wallets take part: balance compares under all / any / not. */
export function FilterWords({
  value,
  choices,
  onChange,
}: {
  value: Logic<FilterLeaf>;
  choices: Choices;
  onChange: (value: Logic<FilterLeaf>) => void;
}) {
  return (
    <LogicEditor<FilterLeaf>
      label="Condition"
      value={value}
      onChange={onChange}
      // One wallet: its total, lowest and highest are its balance, its count 1.
      leaves={[{ value: 'compare:sum', label: 'the balance of' }]}
      kindOf={() => 'compare:sum'}
      make={(k, from) => ({ compare: compareOf(k, from?.compare) })}
      groups={GROUPS}
      empty={{ all: 'every wallet takes part', any: null }}
      leaf={(l, set) => (
        <CompareWords
          value={l.compare}
          choices={choices}
          onChange={(compare) => set({ compare })}
          perWallet
        />
      )}
    />
  );
}

/** When it acts: balance compares and a wait since its last run, under all / any / not. */
export function CheckWords({
  value,
  choices,
  onChange,
}: {
  value: Logic<GateLeaf>;
  choices: Choices;
  onChange: (value: Logic<GateLeaf>) => void;
}) {
  return (
    <LogicEditor<GateLeaf>
      label="Condition"
      value={value}
      onChange={onChange}
      leaves={[...COMPARES, { value: 'since', label: 'the time since its last run' }]}
      kindOf={(g) => ('since' in g ? 'since' : `compare:${g.compare.aggregate}`)}
      make={(k, from) =>
        k === 'since'
          ? { since: { seconds: 3600 } }
          : { compare: compareOf(k, from && 'compare' in from ? from.compare : undefined) }
      }
      groups={GROUPS}
      empty={{ all: 'it always acts', any: null }}
      leaf={(g, set) =>
        'since' in g ? (
          <>
            <Word>is at least</Word>
            <TextField
              fullWidth={false}
              type="number"
              value={g.since.seconds}
              onChange={(e) =>
                set({
                  since: { seconds: Math.max(0, Math.floor(Number(e.target.value)) || 0) },
                })
              }
              slotProps={{ htmlInput: { min: 0, 'aria-label': 'Seconds' } }}
              sx={{ width: 110 }}
            />
            <Word>seconds ({duration(g.since.seconds)})</Word>
          </>
        ) : (
          <CompareWords
            value={g.compare}
            choices={choices}
            onChange={(compare) => set({ compare })}
          />
        )
      }
    />
  );
}

/** A wallet to pick: its currency's symbol, its address cut in the middle, and "watch only" when no key is kept. */
function WalletLine({
  wallet,
  short = false,
}: {
  wallet: { address: string; chain: string; ticker: string; keyed: boolean };
  short?: boolean;
}) {
  const a = wallet.address;
  const cut = short ? 6 : 10;
  return (
    <Box
      component="span"
      sx={{ display: 'inline-flex', alignItems: 'center', gap: 1, minWidth: 0 }}
    >
      <Box component="span" sx={{ display: 'flex', fontSize: 15 }}>
        <Currency chain={wallet.chain} ticker={wallet.ticker} label={false} />
      </Box>
      <Box component="span" sx={{ fontFamily: mono, fontSize: 12.5 }} title={a}>
        {a.length > cut * 2 + 1 ? `${a.slice(0, cut)}…${a.slice(-cut + 2)}` : a}
      </Box>
      {!short && !wallet.keyed && (
        <Box component="span" sx={{ fontSize: 11, color: 'text.secondary' }}>
          watch only
        </Box>
      )}
    </Box>
  );
}

/** The "no tag" choice of the tag picker: wallets whose address carries no tag. */
const NO_TAG = '\u0000no-tag';

/** What a new "tagged" pick starts as: the first tag, or "no tag" while there is none. */
function firstTag(choices: Choices): Pick {
  const tag = choices.tags[0];
  return tag === undefined ? { untagged: {} } : { tag };
}
