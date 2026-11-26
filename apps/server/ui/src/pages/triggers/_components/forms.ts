// A trigger as the API writes it (the server's `models::trigger`), the
// one-line summaries the editor shows for each part, and templates.
import type { Wallet } from '@hashinbox/client';

/** A selector leaf: wallet indexes by id, by asset, or by tag. */
export type Pick =
  { ids: string[] } | { asset: string } | { tag: string } | { untagged: Record<string, never> };

/** AND / OR / NOT over leaves; a leaf is written bare. */
export type Logic<L> = L | { all: Logic<L>[] } | { any: Logic<L>[] } | { not: Logic<L> };

/** Which wallet indexes a trigger acts on (the server's selector JSON). */
export type Selector = Logic<Pick>;

export type Target = { this: { asset?: string } } | { select: Selector };

/** Which balance a read takes: confirmed (the default), or with what is pending in and out. */
export type Balance = 'confirmed' | 'pending';

export type Amount =
  | { value: string }
  | { balance: { of?: Target; balance?: Balance } }
  | { fee: { of?: Target; to?: Target } }
  | { add: [Amount, Amount] }
  | { sub: [Amount, Amount] }
  | { min: [Amount, Amount] }
  | { max: [Amount, Amount] }
  | { times: [Amount, string] };

export type Aggregate = 'sum' | 'min' | 'max' | 'count';
export type Operator = 'above' | 'at_least' | 'below' | 'at_most' | 'equal';

export interface Compare {
  of: Target;
  aggregate: Aggregate;
  balance?: Balance;
  operator: Operator;
  value: Amount;
}

export interface FilterLeaf {
  compare: Compare;
}
export type GateLeaf = { since: { seconds: number } } | { compare: Compare };

/** Moves `amount` of `asset` from `source` to `destination`; one side may be many. */
export interface Send {
  source: Target;
  asset: string;
  destination: Target;
  amount: Amount;
}

/** An action by name with its own input; `send` is the one there is. */
export interface Action {
  name: 'send';
  input: Send;
}

/** A trigger as written: the API's definition. */
export interface Definition {
  id: string;
  chain: string;
  network: string;
  selector: Selector;
  filter: Logic<FilterLeaf> | null;
  check: Logic<GateLeaf>;
  action: Action;
  enabled: boolean;
}

/** How an aggregate compares with its amount: a symbol for summaries, words for menus. */
export const OPERATORS: { value: Operator; symbol: string; label: string }[] = [
  { value: 'above', symbol: '>', label: 'more than' },
  { value: 'at_least', symbol: '≥', label: 'at least' },
  { value: 'below', symbol: '<', label: 'less than' },
  { value: 'at_most', symbol: '≤', label: 'at most' },
  { value: 'equal', symbol: '=', label: 'exactly' },
];

/** What a compare reads of its wallets, in words. */
export const AGGREGATES: { value: Aggregate; label: string }[] = [
  { value: 'sum', label: 'total balance of' },
  { value: 'min', label: 'lowest balance of' },
  { value: 'max', label: 'highest balance of' },
  { value: 'count', label: 'number of' },
];

/** What the pickers offer: the network's assets, wallets and tags. */
export interface Choices {
  /** What "this" (the wallets in context) is called where the picker is. */
  self: string;
  chain: string;
  network: string;
  assets: string[];
  /** What an asset is called: its ticker, else its id shortened. */
  name: (asset: string) => string;
  wallets: Wallet[];
  tags: string[];
}

/** The id of the wallet index of `address` in `asset`. */
export function indexId(chain: string, network: string, address: string, asset: string): string {
  return `${chain}.${network}.${address}.${asset}`;
}

/** Every wallet index id of the network's wallets, with a label. */
export function indexes(
  choices: Choices,
): { id: string; label: string; address: string; chain: string; ticker: string; keyed: boolean }[] {
  return choices.wallets
    .filter((w) => w.chain === choices.chain && w.network === choices.network)
    .flatMap((w) =>
      Object.keys(w.indexes).map((asset) => ({
        id: indexId(choices.chain, choices.network, w.address, asset),
        label: `${w.address.slice(0, 10)}… · ${choices.name(asset)}${w.keyed ? '' : ' · watch only'}`,
        address: w.address,
        chain: choices.chain,
        ticker: choices.name(asset),
        keyed: w.keyed,
      })),
    );
}

/** A trigger id the server takes: 1 to 64 of a-z, 0-9 and -. */
export const validId = (id: string) => /^[a-z0-9-]{1,64}$/.test(id);

// ---- Logic nodes ---------------------------------------------------------------

/** One logic node opened up: a combinator with its children, or a leaf. */
export type Node<L> =
  | { kind: 'all' | 'any'; children: Logic<L>[] }
  | { kind: 'not'; child: Logic<L> }
  | { kind: 'leaf'; leaf: L };

export function nodeOf<L extends object>(logic: Logic<L>): Node<L> {
  if ('all' in logic) return { kind: 'all', children: logic.all };
  if ('any' in logic) return { kind: 'any', children: logic.any };
  if ('not' in logic) return { kind: 'not', child: logic.not };
  return { kind: 'leaf', leaf: logic };
}

/** The first key of a JSON enum value: its variant. */
export const variant = (value: object) => Object.keys(value)[0] ?? '';

/**
 * `logic` turned into `kind`: a combinator keeps what it holds (a leaf
 * becomes its one child); a leaf keeps a child of that kind, else is
 * `make`'s, from the leaf it replaces when there is one.
 */
export function rekind<L extends object>(
  logic: Logic<L>,
  kind: string,
  kindOf: (leaf: L) => string,
  make: (kind: string, from?: L) => L,
): Logic<L> {
  const node = nodeOf(logic);
  const held = node.kind === 'all' || node.kind === 'any' ? node.children : [logic];
  if (kind === 'all') return { all: node.kind === 'not' ? [node.child] : held };
  if (kind === 'any') return { any: node.kind === 'not' ? [node.child] : held };
  if (kind === 'not') return { not: node.kind === 'not' ? node.child : logic };
  const leaves = held.flatMap((h) => {
    const n = nodeOf(h);
    return n.kind === 'leaf' ? [n.leaf] : [];
  });
  return leaves.find((l) => kindOf(l) === kind) ?? make(kind, leaves[0]);
}

/** The two sides of a two-sided amount; null for the others. */
export function sides(amount: Amount): [Amount, Amount] | null {
  if ('add' in amount) return amount.add;
  if ('sub' in amount) return amount.sub;
  if ('min' in amount) return amount.min;
  if ('max' in amount) return amount.max;
  return null;
}

/** `amount` turned into `kind`, keeping its sides and target where it can. */
export function rekindAmount(amount: Amount, kind: string): Amount {
  if (kind === variant(amount)) return amount;
  const held = sides(amount) ?? ('times' in amount ? [amount.times[0]] : [amount]);
  const of = 'balance' in amount ? amount.balance.of : 'fee' in amount ? amount.fee.of : undefined;
  const target = of ?? { this: {} };
  const pair: [Amount, Amount] = [held[0], held[1] ?? { value: '0' }];
  switch (kind) {
    case 'value':
      return held.find((h) => 'value' in h) ?? { value: '0' };
    case 'balance':
      return { balance: { of: target } };
    case 'fee':
      return {
        fee: { of: target, ...('fee' in amount && amount.fee.to && { to: amount.fee.to }) },
      };
    case 'times':
      return { times: [held[0], '1'] };
    default:
      return paired(kind, pair);
  }
}

/** The two-sided amount `kind` (add, sub, min, max) of `pair`. */
export function paired(kind: string, pair: [Amount, Amount]): Amount {
  if (kind === 'add') return { add: pair };
  if (kind === 'sub') return { sub: pair };
  if (kind === 'min') return { min: pair };
  return { max: pair };
}

// ---- One-line summaries ----------------------------------------------------------

type Name = (asset: string) => string;

export function describeLogic<L extends object>(logic: Logic<L>, leaf: (l: L) => string): string {
  const node = nodeOf(logic);
  const inner = (l: Logic<L>) => {
    const text = describeLogic(l, leaf);
    const k = nodeOf(l).kind;
    return k === 'all' || k === 'any' ? `(${text})` : text;
  };
  switch (node.kind) {
    case 'leaf':
      return leaf(node.leaf);
    case 'not':
      return `not ${inner(node.child)}`;
    case 'all':
      return node.children.length === 0 ? 'always' : node.children.map(inner).join(' and ');
    case 'any':
      return node.children.length === 0 ? 'never' : node.children.map(inner).join(' or ');
  }
}

const plural = (n: number, one: string) => `${n} ${one}${n === 1 ? '' : 's'}`;

/** A selector as words: "tagged deposit and in USDC", "every wallet". */
export function describeSelector(selector: Selector, name: Name): string {
  const text = describeLogic(selector, (p) =>
    'ids' in p
      ? plural(p.ids.length, 'picked wallet')
      : 'asset' in p
        ? `in ${name(p.asset)}`
        : 'untagged' in p
          ? 'without a tag'
          : `tagged ${p.tag || '…'}`,
  );
  return text === 'always' ? 'every wallet' : text === 'never' ? 'no wallet' : text;
}

/** "this wallet" (as `self` says), in an asset maybe, or the chosen wallets. */
export function describeTarget(target: Target | undefined, name: Name, self: string): string {
  if (!target || 'this' in target) {
    const asset = target?.this.asset;
    return asset === undefined ? self : `${self} in ${name(asset)}`;
  }
  const text = describeSelector(target.select, name);
  return text.endsWith('wallet') || text.endsWith('wallets') ? text : `wallets ${text}`;
}

export function describeAmount(amount: Amount, name: Name, self: string): string {
  const d = (a: Amount) => describeAmount(a, name, self);
  if ('value' in amount) return amount.value || '0';
  if ('balance' in amount) return `balance of ${describeTarget(amount.balance.of, name, self)}`;
  if ('fee' in amount) {
    const to = amount.fee.to;
    const recipient = to ? ` to ${describeTarget(to, name, self)}` : '';
    return `fee to send all of ${describeTarget(amount.fee.of, name, self)}${recipient}`;
  }
  if ('add' in amount) return `(${d(amount.add[0])} + ${d(amount.add[1])})`;
  if ('sub' in amount) return `(${d(amount.sub[0])} − ${d(amount.sub[1])})`;
  if ('min' in amount) return `lower of (${d(amount.min[0])}, ${d(amount.min[1])})`;
  if ('times' in amount) return `${d(amount.times[0])} × ${amount.times[1] || '?'}`;
  return `higher of (${d(amount.max[0])}, ${d(amount.max[1])})`;
}

export function describeCompare(c: Compare, name: Name, self: string): string {
  const op = OPERATORS.find((o) => o.value === c.operator)?.symbol ?? c.operator;
  const of = AGGREGATES.find((a) => a.value === c.aggregate)?.label ?? c.aggregate;
  return `${of} ${describeTarget(c.of, name, self)} ${op} ${describeAmount(c.value, name, self)}`;
}

/** `seconds` as people read a wait: 1d 2h, 30m, 45s. */
export function duration(seconds: number): string {
  const parts: string[] = [];
  let rest = seconds;
  for (const [unit, size] of [
    ['d', 86400],
    ['h', 3600],
    ['m', 60],
    ['s', 1],
  ] as const) {
    if (rest >= size) {
      parts.push(`${Math.floor(rest / size)}${unit}`);
      rest %= size;
    }
  }
  return parts.join(' ') || '0s';
}

export function describeGate(gate: GateLeaf, name: Name, self: string): string {
  return 'since' in gate
    ? `at least ${duration(gate.since.seconds)} since its last run`
    : describeCompare(gate.compare, name, self);
}

/** What `this` is called in each step. */
export const SELF = {
  /** The filter tests one wallet at a time; so does a send's amount. */
  one: 'this wallet',
  /** The check, and a send's sides, see every wallet the filter kept. */
  kept: 'the kept wallets',
} as const;

export function describeSend(send: Send, name: Name): string {
  return `send ${name(send.asset)} from ${describeTarget(send.source, name, SELF.kept)} to ${describeTarget(send.destination, name, SELF.kept)}, ${describeAmount(send.amount, name, SELF.one)} each`;
}

/** Chosen in the form: the recipient, or the wallet paying gas. */
const chosen: Target = { select: { ids: [] } };

// ---- Amounts the templates use ---------------------------------------------------

// Written as the server writes them back.
const BALANCE: Amount = { balance: { of: { this: {} } } };
/**
 * The fee of sending everything `of` holds to the chosen wallets: priced for
 * them, which costs less when they hold the asset already than for a new
 * address. The chosen wallets are where the template's money goes.
 */
const fee = (of: Target = { this: {} }): Amount => ({ fee: { of, to: chosen } });
const FEE: Amount = fee();
/** Twice the fee of sending everything it holds. */
const TWO_FEES: Amount = { add: [FEE, FEE] };
/** Everything it holds less the fee of sending it. */
const ALL_LESS_FEE: Amount = { sub: [BALANCE, FEE] };
/** The native a `token` send lacks for its fee; never below 0. */
/** This index's balance with what is already on its way, in and out. */
const EXPECTED: Amount = { balance: { of: { this: {} }, balance: 'pending' } };
/** What this index lacks for its token's transfer fee, counting gas already on its way. */
/**
 * What this index lacks for its token's transfer fee with headroom: up to 1.25 × the fee,
 * counting gas already on its way. Topped up only below 1 × (the filter), so a small rise in the
 * gas price before the sweep needs no second top-up.
 */
const shortfall = (token: string): Amount => ({
  max: [{ sub: [{ times: [fee({ this: { asset: token } }), '1.25'] }, EXPECTED] }, { value: '0' }],
});

// ---- Blank triggers and templates -----------------------------------------------

/** A new trigger on `chain`/`network` that does nothing yet: every native index, a send to fill in. */
export function blank(chain: string, network: string): Definition {
  return {
    id: '',
    chain,
    network,
    selector: { asset: 'native' },
    filter: null,
    check: { all: [] },
    action: {
      name: 'send',
      input: {
        source: { this: {} },
        asset: 'native',
        destination: { select: { ids: [] } },
        amount: BALANCE,
      },
    },
    enabled: false,
  };
}

/** Where a template is applied: a network, and the token it acts on when it needs one. */
export interface Place {
  chain: string;
  network: string;
  /** The network's first token, or empty. */
  token: string;
}

/** A named set of triggers to start from; every part stays editable. */
export interface Template {
  name: string;
  description: string;
  /** Whether it acts on a token (it is then offered only where one is configured). */
  token: boolean;
  make: (place: Place) => Definition[];
}

/** The wallets tagged `tag`, in `asset`. */
const tagged = (tag: string, asset: string): Selector => ({ all: [{ tag }, { asset }] });

const native = (place: Place, id: string): Omit<Definition, 'filter' | 'check' | 'action'> => ({
  id,
  chain: place.chain,
  network: place.network,
  selector: tagged('deposit', 'native'),
  enabled: false,
});

export const TEMPLATES: Template[] = [
  {
    name: 'Native forward',
    description:
      'Each address tagged deposit forwards everything it holds, less the fee, once it holds more than twice the fee: the fee then takes at most half.',
    token: false,
    make: (place) => [
      {
        ...native(place, 'forward'),
        // More than twice the fee, not just more than it: the fee reserved is
        // a ceiling, so a forward leaves dust worth about one fee, and dust
        // just above the fee would be forwarded for almost nothing.
        filter: {
          compare: { of: { this: {} }, aggregate: 'sum', operator: 'above', value: TWO_FEES },
        },
        check: { all: [] },
        action: {
          name: 'send',
          input: {
            source: { this: {} },
            asset: 'native',
            destination: chosen,
            amount: ALL_LESS_FEE,
          },
        },
      },
    ],
  },
  {
    name: 'Consolidation over a threshold',
    description:
      'When the addresses tagged deposit hold at least the threshold together, each worth spending sends everything less its fee.',
    token: false,
    make: (place) => [
      {
        ...native(place, 'gather'),
        filter: { compare: { of: { this: {} }, aggregate: 'sum', operator: 'above', value: FEE } },
        check: {
          compare: {
            of: { this: {} },
            aggregate: 'sum',
            operator: 'at_least',
            value: { value: '1' },
          },
        },
        action: {
          name: 'send',
          input: {
            source: { this: {} },
            asset: 'native',
            destination: chosen,
            amount: ALL_LESS_FEE,
          },
        },
      },
    ],
  },
  {
    name: 'Token sweep with gas funding',
    description:
      'Two triggers. fund: an address tagged deposit holding the token but too little native for its transfer gets exactly the shortfall from the gas wallet. sweep: one that has the gas sends all the token. For any address one of them applies, never both.',
    token: true,
    make: (place) => [
      {
        ...native(place, 'fund-gas'),
        filter: {
          all: [
            {
              compare: {
                of: { this: { asset: place.token } },
                aggregate: 'sum',
                operator: 'above',
                value: { value: '0' },
              },
            },
            {
              // Gas already on its way counts: never prefunded twice.
              compare: {
                of: { this: {} },
                aggregate: 'sum',
                balance: 'pending',
                operator: 'below',
                value: fee({ this: { asset: place.token } }),
              },
            },
          ],
        },
        check: { all: [] },
        action: {
          name: 'send',
          input: {
            source: chosen,
            asset: 'native',
            destination: { this: {} },
            amount: shortfall(place.token),
          },
        },
      },
      {
        ...native(place, 'sweep-token'),
        selector: tagged('deposit', place.token),
        // Something to sweep first: the fee of sending none is never asked.
        filter: {
          all: [
            {
              compare: {
                of: { this: {} },
                aggregate: 'sum',
                operator: 'above',
                value: { value: '0' },
              },
            },
            {
              compare: {
                of: { this: { asset: 'native' } },
                aggregate: 'sum',
                operator: 'at_least',
                value: FEE,
              },
            },
          ],
        },
        check: { all: [] },
        action: {
          name: 'send',
          input: {
            source: { this: {} },
            asset: place.token,
            destination: chosen,
            amount: BALANCE,
          },
        },
      },
    ],
  },
  {
    name: 'Scheduled transfer',
    description: 'The address tagged treasury sends a fixed amount at most once a day.',
    token: false,
    make: (place) => [
      {
        ...native(place, 'daily'),
        selector: tagged('treasury', 'native'),
        filter: null,
        check: { since: { seconds: 86400 } },
        action: {
          name: 'send',
          input: {
            source: { this: {} },
            asset: 'native',
            destination: chosen,
            amount: { value: '0.1' },
          },
        },
      },
    ],
  },
];
