// Scopes as text (`com.hashinbox.wallets[x].$read`), checked by the SDK's
// port of `packages/scope`, so the picker offers only what is held.
import { Scopes, type Packed } from '@hashinbox/client';

export const ROOT = 'com.hashinbox';

/** Each resource the server checks and its actions: `wallets` → `read`, `write`. */
export type Resources = [string, string[]][];

/** Every scope of `packed`, written in full. */
export function unpack(packed: Packed): string[] {
  return Object.entries(packed).flatMap(([prefix, list]) =>
    list.map((scope) => (prefix ? `${prefix}.${scope}` : scope)),
  );
}

/** `scopes` packed under `com.hashinbox`, as the server takes them. */
export function pack(scopes: string[]): Packed {
  return { [ROOT]: scopes.map(short) };
}

/** A scope without `com.hashinbox.`. */
export function short(scope: string): string {
  return scope.startsWith(`${ROOT}.`) ? scope.slice(ROOT.length + 1) : scope;
}

/** A scope with `com.hashinbox.`. */
export function full(scope: string): string {
  return scope.startsWith(`${ROOT}.`) ? scope : `${ROOT}.${scope}`;
}

/** Its path steps (ids keep their dots) and its action. */
function parts(scope: string): { path: string[]; action: string } {
  const at = scope.lastIndexOf('.$');
  const text = at < 0 ? scope : scope.slice(0, at);
  const path: string[] = [];
  let depth = 0;
  let start = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (c === '[') depth++;
    if (c === ']') depth--;
    if (c === '.' && depth === 0) {
      path.push(text.slice(start, i));
      start = i + 1;
    }
  }
  path.push(text.slice(start));
  return { path, action: at < 0 ? '' : scope.slice(at + 2) };
}

/** Whether one of `held` allows everything `wanted` allows, as the server checks it. */
export function allows(held: string[], wanted: string): boolean {
  return Scopes.parse(held.join(' ')).covers(wanted);
}

/** What a scope allows, in words: `read wallets`, `everything`. */
export function readable(scope: string): string {
  const { path, action } = parts(short(scope));
  const what = path.join(' › ').replace(/(^| › )\*$/, '$1everything');
  if (action === '*') return what === 'everything' ? 'everything' : `read and write ${what}`;
  return `${action} ${what}`;
}

/** The resources and actions the server's routes ask for, from its OpenAPI document. */
export function resourcesOf(document: unknown): Resources {
  const byResource = new Map<string, Set<string>>();
  const paths = (document as { paths?: Record<string, Record<string, unknown>> }).paths ?? {};
  for (const item of Object.values(paths))
    for (const operation of Object.values(item)) {
      const security = (operation as { security?: Record<string, string[]>[] }).security ?? [];
      for (const scope of security.flatMap((r) => Object.values(r).flat())) {
        if (scope.includes('[')) continue;
        const { path, action } = parts(short(scope));
        const resource = path.join('.');
        byResource.set(resource, (byResource.get(resource) ?? new Set()).add(action));
      }
    }
  return [...byResource.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([resource, actions]) => [resource, [...actions].sort()]);
}
