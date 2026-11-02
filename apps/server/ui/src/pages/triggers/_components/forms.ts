// A trigger as the API writes it, and the simple forms the editor shows
// for it. A part the forms cannot express stays as written and is edited
// as JSON; nothing here drops what it does not understand.

/** Which wallet indexes a trigger acts on (the server's selector JSON). */
export type Selector =
  | { ids: string[] }
  | { asset: string }
  | { tag: string }
  | { all: Selector[] }
  | { any: Selector[] }
  | { not: Selector };

export type Target = { this: { asset?: string } } | { select: Selector };

export type Amount =
  | { value: string }
  | { balance: { of?: Target } }
  | { fee: { of?: Target } }
  | { add: [Amount, Amount] }
  | { sub: [Amount, Amount] }
  | { min: [Amount, Amount] }
  | { max: [Amount, Amount] };

export type Aggregate = 'sum' | 'min' | 'max' | 'count';
export type Operator = 'above' | 'at_least' | 'below' | 'at_most' | 'equal';

export interface Compare {
  of?: Target;
  aggregate: Aggregate;
  operator: Operator;
  value: Amount;
}

export interface FilterLeaf {
  compare: Compare;
}
export type GateLeaf = 'idle' | { since: { seconds: number } } | { compare: Compare };
export type Logic<L> = L | { all: Logic<L>[] } | { any: Logic<L>[] } | { not: Logic<L> };

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

export const OPERATORS: { value: Operator; label: string }[] = [
  { value: 'above', label: '>' },
  { value: 'at_least', label: '≥' },
  { value: 'below', label: '<' },
  { value: 'at_most', label: '≤' },
  { value: 'equal', label: '=' },
];

export const AGGREGATES: Aggregate[] = ['sum', 'min', 'max', 'count'];

// ---- Selector form ----------------------------------------------------------

/** Tags, an asset and excluded tags: the selector most triggers need. */
export interface SelectorForm {
  /** Only this asset; empty: any. */
  asset: string;
  tags: string[];
  /** Whether an index needs every tag, or any one. */
  match: 'all' | 'any';
  /** Never an address with one of these tags. */
  exclude: string[];
}

export function compileSelector(form: SelectorForm): Selector {
  const parts: Selector[] = [];
  if (form.asset) parts.push({ asset: form.asset });
  if (form.match === 'all' || form.tags.length === 1) {
    for (const tag of form.tags) parts.push({ tag });
  } else if (form.tags.length > 1) {
    parts.push({ any: form.tags.map((tag) => ({ tag })) });
  }
  if (form.exclude.length > 0) {
    parts.push({ not: { any: form.exclude.map((tag) => ({ tag })) } });
  }
  return parts.length === 1 && parts[0] ? parts[0] : { all: parts };
}

/** The form `selector` was compiled from; null when it is not one. */
export function parseSelector(selector: Selector): SelectorForm | null {
  const form: SelectorForm = { asset: '', tags: [], match: 'all', exclude: [] };
  const parts = 'all' in selector ? selector.all : [selector];
  for (const part of parts) {
    if ('asset' in part && !form.asset) form.asset = part.asset;
    else if ('tag' in part) form.tags.push(part.tag);
    else if ('any' in part && form.tags.length === 0 && part.any.every((p) => 'tag' in p)) {
      form.tags = part.any.map((p) => ('tag' in p ? p.tag : ''));
      form.match = 'any';
    } else if (
      'not' in part &&
      'any' in part.not &&
      form.exclude.length === 0 &&
      part.not.any.every((p) => 'tag' in p)
    ) {
      form.exclude = part.not.any.map((p) => ('tag' in p ? p.tag : ''));
    } else return null;
  }
  return JSON.stringify(compileSelector(form)) === JSON.stringify(selector) ? form : null;
}

// ---- Compare form -----------------------------------------------------------

/** One compare of the wallet indexes in context against a simple amount. */
export interface CompareForm {
  /** Read the same addresses in this asset; empty: as they are. */
  asset: string;
  aggregate: Aggregate;
  operator: Operator;
  /** A fixed value, the fee of sending all, or the balance itself. */
  amount: 'value' | 'fee' | 'balance';
  /** Display units, when `amount` is `value`. */
  value: string;
}

export function compileCompare(form: CompareForm): Compare {
  const value: Amount =
    form.amount === 'fee'
      ? { fee: { of: { this: {} } } }
      : form.amount === 'balance'
        ? { balance: { of: { this: {} } } }
        : { value: form.value.trim() || '0' };
  return {
    of: { this: form.asset ? { asset: form.asset } : {} },
    aggregate: form.aggregate,
    operator: form.operator,
    value,
  };
}

export function parseCompare(compare: Compare): CompareForm | null {
  const of = compare.of ?? { this: {} };
  if (!('this' in of)) return null;
  const v = compare.value;
  const plain = (target?: Target) =>
    !target || ('this' in target && target.this.asset === undefined);
  let amount: CompareForm['amount'];
  let value = '';
  if ('value' in v) {
    amount = 'value';
    value = v.value;
  } else if ('fee' in v && plain(v.fee.of)) amount = 'fee';
  else if ('balance' in v && plain(v.balance.of)) amount = 'balance';
  else return null;
  return {
    asset: of.this.asset ?? '',
    aggregate: compare.aggregate,
    operator: compare.operator,
    amount,
    value,
  };
}

// ---- Filter and check forms -------------------------------------------------

/** Leaves under one AND, as the forms write them. */
function leaves<L>(logic: Logic<L> | null, isLeaf: (value: unknown) => value is L): L[] | null {
  if (logic === null) return [];
  if (isLeaf(logic)) return [logic];
  if (typeof logic === 'object' && 'all' in logic) {
    const all = logic.all;
    return all.every(isLeaf) ? all : null;
  }
  return null;
}

function isCompareLeaf(value: unknown): value is { compare: Compare } {
  return typeof value === 'object' && value !== null && 'compare' in value;
}

function isGateLeaf(value: unknown): value is GateLeaf {
  return (
    value === 'idle' ||
    isCompareLeaf(value) ||
    (typeof value === 'object' && value !== null && 'since' in value)
  );
}

/** Every compare must hold; empty: no filter. */
export type FilterForm = CompareForm[];

export function compileFilter(form: FilterForm): Logic<FilterLeaf> | null {
  const all = form.map((c) => ({ compare: compileCompare(c) }));
  if (all.length === 0) return null;
  return all.length === 1 && all[0] ? all[0] : { all };
}

export function parseFilter(filter: Logic<FilterLeaf> | null): FilterForm | null {
  const found = leaves(filter, isCompareLeaf);
  if (!found) return null;
  const forms = found.map((leaf) => parseCompare(leaf.compare));
  return forms.every((f) => f !== null) ? forms : null;
}

/** When a trigger acts: not while a run is in flight, a pause, and compares. */
export interface CheckForm {
  idle: boolean;
  /** Seconds since the last run started; 0: none. */
  since: number;
  compares: CompareForm[];
}

export function compileCheck(form: CheckForm): Logic<GateLeaf> {
  const all: GateLeaf[] = [];
  if (form.idle) all.push('idle');
  if (form.since > 0) all.push({ since: { seconds: form.since } });
  for (const c of form.compares) all.push({ compare: compileCompare(c) });
  return all.length === 1 && all[0] !== undefined ? all[0] : { all };
}

export function parseCheck(check: Logic<GateLeaf>): CheckForm | null {
  const found = leaves(check, isGateLeaf);
  if (!found) return null;
  const form: CheckForm = { idle: false, since: 0, compares: [] };
  for (const leaf of found) {
    if (leaf === 'idle') form.idle = true;
    else if ('since' in leaf) form.since = leaf.since.seconds;
    else {
      const c = parseCompare(leaf.compare);
      if (!c) return null;
      form.compares.push(c);
    }
  }
  return JSON.stringify(compileCheck(form)) === JSON.stringify(check) ? form : null;
}

// ---- Targets and amounts -----------------------------------------------------

/** A target as the action editor shows it. */
export type TargetForm =
  | { kind: 'this'; asset: string }
  | { kind: 'ids'; ids: string[] }
  | { kind: 'tag'; tag: string; asset: string };

export function compileTarget(form: TargetForm): Target {
  if (form.kind === 'this') return { this: form.asset ? { asset: form.asset } : {} };
  if (form.kind === 'ids') return { select: { ids: form.ids } };
  const tag: Selector = { tag: form.tag };
  return { select: form.asset ? { all: [tag, { asset: form.asset }] } : tag };
}

export function parseTarget(target: Target): TargetForm | null {
  if ('this' in target) return { kind: 'this', asset: target.this.asset ?? '' };
  const s = target.select;
  if ('ids' in s) return { kind: 'ids', ids: s.ids };
  if ('tag' in s) return { kind: 'tag', tag: s.tag, asset: '' };
  if ('all' in s && s.all.length === 2) {
    const [a, b] = s.all;
    if (a && b && 'tag' in a && 'asset' in b) return { kind: 'tag', tag: a.tag, asset: b.asset };
  }
  return null;
}

/** The amounts most sends need, by name. */
export type AmountForm =
  | { kind: 'all' }
  | { kind: 'all-less-fee' }
  | { kind: 'shortfall'; asset: string }
  | { kind: 'value'; value: string };

// Written as the server writes them back, so a stored trigger parses again.
const BALANCE: Amount = { balance: { of: { this: {} } } };
const FEE: Amount = { fee: { of: { this: {} } } };

export function compileAmount(form: AmountForm): Amount {
  switch (form.kind) {
    case 'all':
      return BALANCE;
    case 'all-less-fee':
      return { sub: [BALANCE, FEE] };
    case 'shortfall':
      return {
        max: [{ sub: [{ fee: { of: { this: { asset: form.asset } } } }, BALANCE] }, { value: '0' }],
      };
    case 'value':
      return { value: form.value.trim() || '0' };
  }
}

export function parseAmount(amount: Amount): AmountForm | null {
  if ('value' in amount) return { kind: 'value', value: amount.value };
  const same = (a: Amount, b: Amount) => JSON.stringify(a) === JSON.stringify(b);
  if (same(amount, BALANCE)) return { kind: 'all' };
  if (same(amount, compileAmount({ kind: 'all-less-fee' }))) return { kind: 'all-less-fee' };
  if ('max' in amount) {
    const [inner] = amount.max;
    if ('sub' in inner) {
      const [fee] = inner.sub;
      if ('fee' in fee && fee.fee.of && 'this' in fee.fee.of && fee.fee.of.this.asset) {
        const form: AmountForm = { kind: 'shortfall', asset: fee.fee.of.this.asset };
        if (same(amount, compileAmount(form))) return form;
      }
    }
  }
  return null;
}

/** The id of the wallet index of `address` in `asset`. */
export function indexId(chain: string, network: string, address: string, asset: string): string {
  return `${chain}.${network}.${address}.${asset}`;
}

// ---- Blank triggers and templates -----------------------------------------------

/** A new trigger on `chain`/`network` that does nothing yet: every native index, a send to fill in. */
export function blank(chain: string, network: string): Definition {
  return {
    id: '',
    chain,
    network,
    selector: { asset: 'native' },
    filter: null,
    check: 'idle',
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

/** Chosen in the form: the recipient, or the wallet paying gas. */
const chosen: Target = { select: { ids: [] } };

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
      'Each address tagged deposit forwards everything it holds, less the fee, once that is worth sending.',
    token: false,
    make: (place) => [
      {
        ...native(place, 'forward'),
        filter: { compare: { of: { this: {} }, aggregate: 'sum', operator: 'above', value: FEE } },
        check: 'idle',
        action: {
          name: 'send',
          input: {
            source: { this: {} },
            asset: 'native',
            destination: chosen,
            amount: compileAmount({ kind: 'all-less-fee' }),
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
          all: [
            'idle',
            {
              compare: {
                of: { this: {} },
                aggregate: 'sum',
                operator: 'at_least',
                value: { value: '1' },
              },
            },
          ],
        },
        action: {
          name: 'send',
          input: {
            source: { this: {} },
            asset: 'native',
            destination: chosen,
            amount: compileAmount({ kind: 'all-less-fee' }),
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
              compare: {
                of: { this: {} },
                aggregate: 'sum',
                operator: 'below',
                value: { fee: { of: { this: { asset: place.token } } } },
              },
            },
          ],
        },
        check: 'idle',
        action: {
          name: 'send',
          input: {
            source: chosen,
            asset: 'native',
            destination: { this: {} },
            amount: compileAmount({ kind: 'shortfall', asset: place.token }),
          },
        },
      },
      {
        ...native(place, 'sweep-token'),
        selector: tagged('deposit', place.token),
        filter: {
          compare: {
            of: { this: { asset: 'native' } },
            aggregate: 'sum',
            operator: 'at_least',
            value: FEE,
          },
        },
        check: 'idle',
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
        check: { all: ['idle', { since: { seconds: 86400 } }] },
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
