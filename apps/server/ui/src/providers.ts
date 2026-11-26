// Known RPC providers, configured in providers.json: each one's docs, how it
// authenticates, and its base URL per network. A provider is a shortcut to
// node URLs; the server only ever sees URLs. A key a provider wants in a
// header is written as a `header.<Name>` query parameter, which the server
// sends as that header.
import providers from './providers.json';

/** How a provider authenticates a call. */
export type Auth =
  /** An API key in this header. */
  | { in: 'header'; name: string }
  /** An API key in this query parameter. */
  | { in: 'query'; name: string }
  /** An API key appended to the base URL's path. */
  | { in: 'path' }
  /** Each endpoint has its own URL and token, pasted whole. */
  | { in: 'endpoint' }
  /** None: a public node. */
  | { in: 'none' };

export interface Provider {
  name: string;
  /** Where its docs describe its endpoints. */
  docs: string;
  auth: Auth;
  /** Base URL by `chain/network` it serves; null when each endpoint's URL is pasted. */
  networks: Record<string, string | null>;
}

export const PROVIDERS = providers as Provider[];

/** Network names as configurations commonly write them, by chain. */
const ALIASES: Record<string, Record<string, string>> = {
  bitcoin: {
    mainnet: 'mainnet',
    bitcoin: 'mainnet',
    testnet: 'testnet',
    testnet3: 'testnet',
    testnet4: 'testnet4',
    signet: 'signet',
  },
  ethereum: { mainnet: 'mainnet', sepolia: 'sepolia', hoodi: 'hoodi' },
  solana: { mainnet: 'mainnet', 'mainnet-beta': 'mainnet', devnet: 'devnet', testnet: 'testnet' },
  tron: { mainnet: 'mainnet', nile: 'nile' },
};

/**
 * `provider`'s base URL for the configured `chain`/`network`: a URL, null
 * when its endpoint URL is pasted, undefined when it does not serve it.
 */
export function baseFor(
  provider: Provider,
  chain: string,
  network: string,
): string | null | undefined {
  const canonical = ALIASES[chain]?.[network];
  return canonical ? provider.networks[`${chain}/${canonical}`] : undefined;
}

/** Whether `provider` asks for an API key. */
export function asksKey(provider: Provider): boolean {
  return ['header', 'query', 'path'].includes(provider.auth.in);
}

/** The node URL `provider` makes from `base` and `key`; null while the key is missing. */
export function urlOf(provider: Provider, base: string, key: string): string | null {
  const value = key.trim();
  const auth = provider.auth;
  if (auth.in === 'none') return base;
  if (auth.in === 'endpoint' || value === '') return null;
  if (auth.in === 'path') return `${base}${encodeURIComponent(value)}`;
  const url = new URL(base);
  url.searchParams.append(auth.in === 'header' ? `header.${auth.name}` : auth.name, value);
  return url.toString();
}
