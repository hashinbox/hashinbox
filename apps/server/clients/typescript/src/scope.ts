// Hand-written: a port of packages/scope (its grammar, coverage, escaping,
// compress, pack and unpack), held to it by ../../_golden/scope.json.

/** One step of a scope's path: a resource (narrowed to `id`, or all), or `*`. */
export type Segment = { name: string; id?: string } | "*";

/** What a scope allows doing: `$name`, or `$*` (`"*"`). */
export type Action = string;

/** Scopes as a JWT packs them: `{"com.hashinbox": ["wallets[x].$read"]}`. */
export type Packed = Record<string, string[]>;

/** Why a scope's text does not parse. */
export class ScopeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ScopeError";
  }
}

/** One scope: `com.hashinbox.wallets[id].$read`. */
export class Scope {
  readonly path: readonly Segment[];
  readonly action: Action;

  constructor(path: readonly Segment[], action: Action) {
    if (path.length === 0) throw new ScopeError("a scope needs a resource");
    this.path = path;
    this.action = action;
  }

  /** The scope `text` is: steps split by `.` (not inside `[` `]`), then `$action`. */
  static parse(text: string): Scope {
    const steps: string[] = [];
    let start = 0;
    let inside = false;
    for (let at = 0; at < text.length; at++) {
      const char = text[at];
      if (char === "[" && !inside) inside = true;
      else if (char === "]" && inside) inside = false;
      else if (char === "." && !inside) {
        steps.push(text.slice(start, at));
        start = at + 1;
      }
    }
    if (inside) throw new ScopeError(`bad id in \`${text}\``);
    const last = text.slice(start);
    if (!last.startsWith("$"))
      throw new ScopeError(`\`${text}\` does not end with \`$action\``);
    const action = last.slice(1);
    if (action !== "*" && !isName(action))
      throw new ScopeError(`\`${action}\` is not a name`);
    return new Scope(steps.map(parseSegment), action);
  }

  /**
   * Whether it allows everything `other` allows: its action the same or
   * `$*`, its path matching step by step (`*` in the middle: any one step;
   * at the end: anything below, itself included).
   */
  covers(other: Scope): boolean {
    const action = this.action === "*" || this.action === other.action;
    return action && pathCovers(this.path, other.path, 0, 0);
  }

  toString(): string {
    return [...this.path.map(segmentText), `$${this.action}`].join(".");
  }
}

/** A set of scopes: what a key or a token holds. */
export class Scopes implements Iterable<Scope> {
  readonly list: readonly Scope[];

  constructor(list: readonly Scope[] = []) {
    this.list = list;
  }

  /** Space-separated scope text (empty: none), or a packed claim. */
  static parse(input: string | Packed): Scopes {
    if (typeof input !== "string") return unpack(input);
    return new Scopes(input.split(WHITESPACE).filter(Boolean).map(Scope.parse));
  }

  [Symbol.iterator](): Iterator<Scope> {
    return this.list[Symbol.iterator]();
  }

  /** Whether one of them covers `scope` (text that does not parse: false). */
  covers(scope: Scope | string): boolean {
    const needed = typeof scope === "string" ? tryParse(scope) : scope;
    return (
      needed !== undefined && this.list.some((held) => held.covers(needed))
    );
  }

  /** The first of `others` none of them covers; `undefined`: they cover all. */
  firstUncovered(others: Scopes): Scope | undefined {
    return others.list.find((scope) => !this.covers(scope));
  }

  /** The fewest scopes allowing the same, sorted: one covered by another dropped. */
  compress(): Scopes {
    const list = this.list;
    const dropped = (at: number) =>
      list.some(
        (held, other) =>
          other !== at &&
          held.covers(list[at]!) &&
          (!list[at]!.covers(held) || other < at),
      );
    const kept = list.filter((_, at) => !dropped(at));
    return new Scopes([...kept].sort(compareScope));
  }

  /** Compressed, then grouped by their shared prefix: a JWT's `scopes` claim. */
  pack(): Packed {
    return Object.fromEntries(packEntries(this));
  }

  toString(): string {
    return this.list.join(" ");
  }
}

/**
 * The packed claim as ordered entries (by prefix, as the server's `BTreeMap`):
 * what a JWT's JSON must keep in that order.
 */
export function packEntries(scopes: Scopes): [string, string[]][] {
  const groups: Scope[][] = [];
  for (const scope of scopes.compress()) {
    const group = groups.find((g) =>
      sameSegment(g[0]!.path[0]!, scope.path[0]!),
    );
    if (group) group.push(scope);
    else groups.push([scope]);
  }
  groups.sort((a, b) => compareSegment(a[0]!.path[0]!, b[0]!.path[0]!));
  const packed = new Map<string, string[]>();
  for (const group of groups) {
    const shared = sharedPrefix(group);
    for (const scope of group) {
      const prefix = scope.path.slice(0, shared).map(segmentText).join(".");
      const rest = new Scope(scope.path.slice(shared), scope.action).toString();
      packed.set(prefix, [...(packed.get(prefix) ?? []), rest]);
    }
  }
  return [...packed].sort(([a], [b]) => compareText(a, b));
}

/** `id` as written between a scope's brackets: `%`, `[`, `]`, whitespace and controls as `%XX`. */
export function escapeId(id: string): string {
  return id.replace(
    /[%[\] \x00-\x1f\x7f]/g,
    (char) =>
      "%" + char.charCodeAt(0).toString(16).toUpperCase().padStart(2, "0"),
  );
}

/**
 * `template` with each `{name}` the text of `params[name]`, through `escape`
 * (a scope's: `escapeId`).
 */
export function fill(
  template: string,
  params: object,
  escape: (text: string) => string = escapeId,
): string {
  const values = params as Record<string, unknown>;
  return template.replace(/\{([^}]+)\}/g, (_, name: string) => {
    const value = values[name];
    if (value === undefined || value === null)
      throw new TypeError(`\`${template}\` needs \`${name}\``);
    return escape(String(value));
  });
}

/** Rust's `char::is_whitespace`, what `Scopes` text splits on. */
const WHITESPACE =
  /[\t\n\v\f\r \x85\xa0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000]+/;

function isName(text: string): boolean {
  return /^[A-Za-z0-9_-]+$/.test(text);
}

function tryParse(text: string): Scope | undefined {
  try {
    return Scope.parse(text);
  } catch {
    return undefined;
  }
}

function parseSegment(text: string): Segment {
  if (text === "*") return "*";
  const open = text.indexOf("[");
  if (open < 0) {
    if (!isName(text)) throw new ScopeError(`\`${text}\` is not a name`);
    return { name: text };
  }
  const name = text.slice(0, open);
  const rest = text.slice(open + 1);
  if (!rest.endsWith("]")) throw new ScopeError(`bad id in \`${name}\``);
  const id = unescape(rest.slice(0, -1));
  if (id === "") throw new ScopeError(`bad id in \`${name}\``);
  if (!isName(name)) throw new ScopeError(`\`${name}\` is not a name`);
  return { name, id };
}

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });

/** The id `text` (as written between `[` and `]`) is; `%XX` as Rust's `u8::from_str_radix` reads it. */
function unescape(text: string): string {
  const bytes = encoder.encode(text);
  const out: number[] = [];
  for (let at = 0; at < bytes.length; at++) {
    if (bytes[at] !== 0x25) {
      out.push(bytes[at]!);
      continue;
    }
    const hex = String.fromCharCode(...bytes.slice(at + 1, at + 3));
    if (!/^(\+[0-9a-fA-F]|[0-9a-fA-F]{2})$/.test(hex))
      throw new ScopeError(`bad escape in \`${text}\``);
    out.push(parseInt(hex, 16));
    at += 2;
  }
  try {
    return decoder.decode(new Uint8Array(out));
  } catch {
    throw new ScopeError(`bad escape in \`${text}\``);
  }
}

function segmentText(segment: Segment): string {
  if (segment === "*") return "*";
  return segment.id === undefined
    ? segment.name
    : `${segment.name}[${escapeId(segment.id)}]`;
}

function segmentCovers(ours: Segment, theirs: Segment): boolean {
  if (ours === "*") return true;
  if (theirs === "*") return false;
  return (
    ours.name === theirs.name &&
    (ours.id === undefined || ours.id === theirs.id)
  );
}

function pathCovers(
  ours: readonly Segment[],
  theirs: readonly Segment[],
  at: number,
  their: number,
): boolean {
  if (at === ours.length && their === theirs.length) return true;
  if (at === ours.length - 1 && ours[at] === "*") return true;
  if (at === ours.length || their === theirs.length) return false;
  return (
    segmentCovers(ours[at]!, theirs[their]!) &&
    pathCovers(ours, theirs, at + 1, their + 1)
  );
}

function sameSegment(a: Segment, b: Segment): boolean {
  return compareSegment(a, b) === 0;
}

/** How many leading steps every scope of `group` shares as the same plain name, each keeping one. */
function sharedPrefix(group: Scope[]): number {
  const shortest = Math.min(...group.map((scope) => scope.path.length));
  const first = group[0]!.path;
  let shared = 0;
  while (shared < shortest - 1) {
    const step = first[shared]!;
    if (step === "*" || step.id !== undefined) break;
    if (!group.every((scope) => sameSegment(scope.path[shared]!, step))) break;
    shared++;
  }
  return shared;
}

/** Text order by code point: Rust's `str` order (UTF-8 bytes). */
function compareText(a: string, b: string): number {
  const x = [...a];
  const y = [...b];
  for (let at = 0; at < Math.min(x.length, y.length); at++) {
    const diff = x[at]!.codePointAt(0)! - y[at]!.codePointAt(0)!;
    if (diff !== 0) return diff;
  }
  return x.length - y.length;
}

/** Rust's derived `Ord` of `Segment`: a name (by name, then id, none first) before `*`. */
function compareSegment(a: Segment, b: Segment): number {
  if (a === "*" || b === "*") return (a === "*" ? 1 : 0) - (b === "*" ? 1 : 0);
  const names = compareText(a.name, b.name);
  if (names !== 0) return names;
  if (a.id === undefined || b.id === undefined)
    return (a.id === undefined ? 0 : 1) - (b.id === undefined ? 0 : 1);
  return compareText(a.id, b.id);
}

/** Rust's derived `Ord` of `Scope`: its path, then its action (a name before `*`). */
function compareScope(a: Scope, b: Scope): number {
  for (let at = 0; at < Math.min(a.path.length, b.path.length); at++) {
    const diff = compareSegment(a.path[at]!, b.path[at]!);
    if (diff !== 0) return diff;
  }
  if (a.path.length !== b.path.length) return a.path.length - b.path.length;
  if (a.action === "*" || b.action === "*")
    return (a.action === "*" ? 1 : 0) - (b.action === "*" ? 1 : 0);
  return compareText(a.action, b.action);
}

/** The scopes a packed claim holds: each prefix and its scopes joined by `.`. */
function unpack(packed: Packed): Scopes {
  const prefixes = Object.keys(packed).sort(compareText);
  const list = prefixes.flatMap((prefix) =>
    packed[prefix]!.map((scope) =>
      Scope.parse(prefix === "" ? scope : `${prefix}.${scope}`),
    ),
  );
  return new Scopes(list);
}
