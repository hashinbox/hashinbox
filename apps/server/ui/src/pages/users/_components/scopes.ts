// Scopes as text (`com.hashinbox.wallets[x]:read`), checked by the SDK's
// port of `packages/scope`, so the picker offers only what is held.
import { DEPENDS, Scope, Scopes, type Packed } from '@hashinbox/client';

export const ROOT = 'com.hashinbox';

/** The scope tree the server's routes ask for, ids dropped, a parent before its children: `wallets`, `wallets.tags`. */
export type Resources = string[];

/** Every scope of `packed`, written in full. */
export function unpack(packed: Packed): string[] {
  return [...Scopes.parse(packed)].map(String);
}

/** `scopes` packed, as the server takes them. */
export function pack(scopes: string[]): Packed {
  return Scopes.parse(scopes.join(' ')).pack();
}

/** A scope without `com.hashinbox.`. */
export function short(scope: string): string {
  return scope.startsWith(`${ROOT}.`) ? scope.slice(ROOT.length + 1) : scope;
}

/** A scope with `com.hashinbox.` (`:read` alone: the root's). */
export function full(scope: string): string {
  if (scope.startsWith(`${ROOT}.`) || scope.startsWith(`${ROOT}:`) || scope === ROOT) return scope;
  return scope.startsWith(':') ? `${ROOT}${scope}` : `${ROOT}.${scope}`;
}

/** Whether `held` (expanded as the server does) allows everything `wanted` allows. */
export function allows(held: string[], wanted: string): boolean {
  return Scopes.parse(held.join(' ')).expand(DEPENDS).covers(wanted);
}

/** What a scope allows, in words: `read wallets › tags`, `everything`. */
export function readable(scope: string): string {
  let parsed: Scope;
  try {
    parsed = Scope.parse(scope);
  } catch {
    return 'unknown';
  }
  const steps = parsed.path.slice(2).map((step) => step.name + (step.id ? ` ${step.id}` : ''));
  const what = steps.length === 0 ? 'everything' : steps.join(' › ');
  return parsed.read ? `read ${what}` : what === 'everything' ? what : `full ${what}`;
}

/** What holding `path` (full) also gives, beyond its subtree: `wallets`. */
export function implied(path: string): string[] {
  return (DEPENDS[`${ROOT}.${path}`] ?? []).map(short);
}

/** The scope tree the server's routes ask for, from its OpenAPI document: each path, ids dropped, and its parents. */
export function resourcesOf(document: unknown): Resources {
  const paths = new Set<string>();
  const routes = (document as { paths?: Record<string, Record<string, unknown>> }).paths ?? {};
  for (const item of Object.values(routes))
    for (const operation of Object.values(item)) {
      const security = (operation as { security?: Record<string, string[]>[] }).security ?? [];
      for (const template of security.flatMap((r) => Object.values(r).flat())) {
        const steps = short(template.replace(/\[[^\]]*\]/g, '').replace(/:read$/, '')).split('.');
        steps.forEach((_, at) => paths.add(steps.slice(0, at + 1).join('.')));
      }
    }
  // A space sorts before every name's letter: a parent before its children.
  const key = (path: string) => path.replaceAll('.', ' ');
  return [...paths].sort((a, b) => (key(a) < key(b) ? -1 : key(a) > key(b) ? 1 : 0));
}
