// Block explorer links, per network, as `GET /v1/networks` gives them; the
// layout provides them, `Address` and `Transaction` read them.
import { createContext, useContext } from 'react';

/** One network's links: `{address}` and `{tx}` are filled in. */
export interface Explorer {
  address: string;
  transaction: string;
}

/** Explorers by `chain/network`. */
export const Explorers = createContext<Map<string, Explorer>>(new Map());

/** The explorer of `network`, when one is configured. */
export function useExplorer(network: { chain: string; name: string }): Explorer | undefined {
  return useContext(Explorers).get(`${network.chain}/${network.name}`);
}
