// Hand-written: a port of packages/scope (its grammar, coverage, escaping,
// compress, pack, unpack and expand), held to it by ../../_golden/scope.json.

/** One step of a scope's path: a resource, narrowed to `id` or all of it. */
export interface Segment {
  name: string;
  id?: string;
}

/**
 * Scopes as a JWT packs them: each prefix and its scopes, one as text, several
 * as a list, `*` the prefix itself: `{"com.hashinbox": ["*:read", "wallets"]}`.
 */
export type Packed = Record<string, string | string[]>;

/** What holding a scope needs beyond its subtree: source → targets (`DEPENDS`). */
export type Depends = Readonly<Record<string, readonly string[]>>;

/** Why a scope's text does not parse. */
export class ScopeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ScopeError";
  }
}

/** One scope: `com.hashinbox.wallets[id]` (full) or `…:read` (read only). */
export class Scope {
  readonly path: readonly Segment[];
  readonly read: boolean;

  constructor(path: readonly Segment[], read: boolean) {
    if (path.length === 0) throw new ScopeError("a scope needs a resource");
    this.path = path;
    this.read = read;
  }

  /** The scope `text` is: steps split by `.` (not inside `[` `]`), then `:read` or nothing. */
  static parse(text: string): Scope {
    const steps: string[] = [];
    let start = 0;
    let end = text.length;
    let inside = false;
    for (let at = 0; at < text.length; at++) {
      const char = text[at];
      if (char === "[" && !inside) inside = true;
      else if (char === "]" && inside) inside = false;
      else if (char === "." && !inside) {
        steps.push(text.slice(start, at));
        start = at + 1;
      } else if (char === ":" && !inside) {
        end = at;
        break;
      }
    }
    if (inside) throw new ScopeError(`bad id in \`${text}\``);
    const level = text.slice(end);
    if (level !== "" && level !== ":read")
      throw new ScopeError(
        `\`${text}\` ends with neither \`:read\` nor nothing`,
      );
    if (text === "") throw new ScopeError("a scope needs a resource");
    steps.push(text.slice(start, end));
    return new Scope(steps.map(parseSegment), level !== "");
  }

  /**
   * Whether it allows everything `other` allows: it is full or `other` is
   * read only, and its path is a prefix of `other`'s, step by step: its
   * whole subtree.
   */
  covers(other: Scope): boolean {
    return (!this.read || other.read) && reaches(this.path, other.path);
  }

  toString(): string {
    return this.path.map(segmentText).join(".") + (this.read ? ":read" : "");
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

  /**
   * Them and every scope `depends` adds, to a fixed point, compressed: a held
   * scope covering a source (its ids ignored) adds each target whole, read
   * only when the target is `:read` or the held scope is.
   */
  expand(depends: Depends): Scopes {
    const table = Object.entries(depends).flatMap(([source, targets]) => {
      const parsed = tryParse(source);
      const each = targets.map(tryParse);
      if (!parsed || each.some((target) => !target)) return [];
      return [[parsed, each as Scope[]] as const];
    });
    const held = [...this.list];
    for (let at = 0; at < held.length; at++) {
      const scope = held[at]!;
      for (const [source, targets] of table) {
        if (!triggers(scope, source)) continue;
        for (const target of targets) {
          const added = new Scope(target.path, target.read || scope.read);
          if (!held.some((kept) => kept.covers(added))) held.push(added);
        }
      }
    }
    return new Scopes(held).compress();
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
export function packEntries(scopes: Scopes): [string, string | string[]][] {
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
      const rest =
        scope.path.length === shared
          ? scope.read
            ? "*:read"
            : "*"
          : new Scope(scope.path.slice(shared), scope.read).toString();
      packed.set(prefix, [...(packed.get(prefix) ?? []), rest]);
    }
  }
  return [...packed]
    .sort(([a], [b]) => compareText(a, b))
    .map(([prefix, list]) => [prefix, list.length === 1 ? list[0]! : list]);
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
  return segment.id === undefined
    ? segment.name
    : `${segment.name}[${escapeId(segment.id)}]`;
}

/** Whether path `ours` is a prefix of `theirs`, step by step. */
function reaches(
  ours: readonly Segment[],
  theirs: readonly Segment[],
): boolean {
  return (
    ours.length <= theirs.length &&
    ours.every(
      (step, at) =>
        step.name === theirs[at]!.name &&
        (step.id === undefined || step.id === theirs[at]!.id),
    )
  );
}

/** Whether `held` triggers `source`'s dependencies: it covers `source`, its ids ignored. */
function triggers(held: Scope, source: Scope): boolean {
  const names = held.path.map((step) => ({ name: step.name }));
  return reaches(names, source.path);
}

function sameSegment(a: Segment, b: Segment): boolean {
  return compareSegment(a, b) === 0;
}

/** How many leading steps every scope of `group` shares as the same plain name, each keeping one. */
function sharedPrefix(group: Scope[]): number {
  const shortest = Math.min(...group.map((scope) => scope.path.length));
  const first = group[0]!.path;
  let shared = 0;
  while (shared < shortest) {
    const step = first[shared]!;
    if (step.id !== undefined) break;
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

/** Rust's derived `Ord` of `Segment`: by name, then id (none first). */
function compareSegment(a: Segment, b: Segment): number {
  const names = compareText(a.name, b.name);
  if (names !== 0) return names;
  if (a.id === undefined || b.id === undefined)
    return (a.id === undefined ? 0 : 1) - (b.id === undefined ? 0 : 1);
  return compareText(a.id, b.id);
}

/** Rust's derived `Ord` of `Scope`: its path, then full before read. */
function compareScope(a: Scope, b: Scope): number {
  for (let at = 0; at < Math.min(a.path.length, b.path.length); at++) {
    const diff = compareSegment(a.path[at]!, b.path[at]!);
    if (diff !== 0) return diff;
  }
  if (a.path.length !== b.path.length) return a.path.length - b.path.length;
  return Number(a.read) - Number(b.read);
}

/** The scopes a packed claim holds: each prefix and its scopes joined by `.`, `*` the prefix itself. */
function unpack(packed: Packed): Scopes {
  const prefixes = Object.keys(packed).sort(compareText);
  const list = prefixes.flatMap((prefix) =>
    [packed[prefix]!].flat().map((scope) => Scope.parse(joined(prefix, scope))),
  );
  return new Scopes(list);
}

/** `scope` under `prefix`: `*` (`*:read`) the prefix itself. */
function joined(prefix: string, scope: string): string {
  if (prefix === "") return scope;
  const level = scope.startsWith("*") ? scope.slice(1) : undefined;
  if (level === "" || level?.startsWith(":")) return prefix + level;
  return `${prefix}.${scope}`;
}
