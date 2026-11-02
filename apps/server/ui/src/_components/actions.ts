// What a hover tooltip (`Hover`) offers: buttons, each a copy, a link or a
// handler. A component has default actions a caller may replace or extend.
import type { ReactNode } from 'react';

/** One button: copies `copy`, opens `href` in a new tab, or runs `onClick`. */
export interface Action {
  label: string;
  icon?: ReactNode;
  copy?: string;
  href?: string;
  onClick?: () => void;
}

/** A caller's change to a component's default actions: a list replaces them, a function edits them. */
export type Actions = Action[] | ((defaults: Action[]) => Action[]);

/** The caller's `actions` applied to `defaults`. */
export function resolve(defaults: Action[], actions?: Actions): Action[] {
  if (!actions) return defaults;
  return typeof actions === 'function' ? actions(defaults) : actions;
}
