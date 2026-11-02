/** Long text, such as an address or a hash, cut to its head and tail. */
export function shorten(text: string): string {
  return text.length > 20 ? `${text.slice(0, 9)}…${text.slice(-6)}` : text;
}
