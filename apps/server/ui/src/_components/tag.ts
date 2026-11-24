/** One tag of a filter: its group (`network`, `kind`, `level`…) and value. */
export interface Tag<G extends string = string> {
  group: G;
  value: string;
  label: string;
}

/** The value of `tags`' tag in `group`; empty for none. */
export function valueOf<G extends string>(tags: Tag<G>[], group: G): string {
  return tags.find((t) => t.group === group)?.value ?? '';
}
