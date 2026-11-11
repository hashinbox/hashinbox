# P2P Atomic Swap Network with Open-Source Matchmaker & Watchtower

Nov 11, 2026

## Summary

Peers swap assets across any two chains with adaptor-signature atomic swaps, and an open-source **Tower** server handles everything except custody: order book, message relay, and watchtower.

- **Nodes** (user software) hold all keys, sign all transactions, and run the swap state machine. Funds only ever sit in 2-of-2 outputs owned by the two peers.
- **Tower** matches makers with takers, relays encrypted messages, and broadcasts pre-signed claim and refund transactions if a peer goes offline.
- **Core rule:** every transaction the Tower can broadcast is already signed and pays only to a peer's own address. A malicious Tower can censor, stall, or leak metadata, but it cannot steal.
- Anyone can deploy a Tower from the open-source code, but each Tower is a standalone market: no gossip, no shared order book, no federation. Both peers in a swap use the same Tower.

## Roles and components

Two kinds of software: nodes that own funds, and a Tower that only coordinates.

&#91;embedded content: architecture · 2 nodes, 1 Tower, 2 chains\]

- **Maker node:** posts orders, answers RFQs, hedges on a CEX, locks second. Usually an always-online bot.
- **Taker node:** a wallet or app. Picks a quote, generates the secret, locks first, claims first.
- **Tower:** an order book with RFQ, an encrypted relay between matched nodes, and a watchtower holding pre-signed transactions.
- **Chain adapters:** one plugin per chain, implementing locks, adaptor signatures and chain I/O (see Chain adapter interface).

## Trust model

The Tower is trusted for liveness and privacy, never for funds.

| Tower behaviour | Possible? | Effect | Mitigation |
| --- | --- | --- | --- |
| Steal funds | No | Holds no keys; every tx it holds pays a peer's own address | Enforced by cryptography |
| Redirect a claim or refund | No | Pre-signed txs commit to the output address | Enforced by cryptography |
| Forge or alter orders | No | Orders are signed by the maker's node key | Nodes verify every signature |
| Censor orders or peers | Yes | Some trades never happen | Users move to another Tower or deploy their own |
| Go offline mid-swap | Yes | Watchtower protection lost | Every node runs its own watcher with the same packages |
| Withhold a broadcast | Yes | Late claim, possible loss | Node's own watcher; generous timeout margins |
| Read swap metadata | Yes | Links who traded what, when | Tor, fresh keys per swap, self-hosted Tower |
| Front-run as a maker | Yes | Takes the best orders itself | Public, append-only order log so selective matching is visible |

## Order book and matchmaking

Makers post signed standing offers, takers pick one, and the Tower reserves it and introduces the two nodes.

**Order (signed by the maker's node identity key):**

```json
{
  "id": "ord_7f3a",
  "maker_pubkey": "02ab…",
  "give":  { "chain": "bitcoin",  "asset": "BTC" },
  "want":  { "chain": "base",     "asset": "USDC" },
  "rate": "68250.00",
  "min": "0.001", "max": "0.5",
  "premium_bps": 10,
  "timeouts": { "maker_lock_s": 1800, "t_short_s": 43200, "t_long_s": 86400 },
  "expires_at": 1762900000,
  "sig": "…"
}
```

**Matching flow:**

1. Maker node posts the order. The Tower verifies the signature and appends it to a public order log.
2. Taker browses orders (filters: pair, size, maker reputation) and sends a signed `take` with amount and its own node key.
3. Tower reserves that amount of the order for a short window (for example 60 s) so two takers can't hit the same liquidity.
4. Tower opens an encrypted session between the two nodes (Noise protocol, end-to-end; the Tower relays ciphertext only).
5. Nodes run the swap protocol through the relay. During setup they also exchange direct endpoints (for example Tor onion addresses), so they can finish the swap without the Tower if it goes down. If setup isn't finished within the window, the reservation lapses and the order returns to the book.

Makers are expected to be always-online bots, which is what fixes the liquidity and "both online" problems for ordinary users.

## Swap protocol

The taker holds the secret, locks first with the long timeout, and claims first. The maker locks second with the short timeout and claims last, using the secret the taker's claim reveals.

Example: taker gives USDC on Base (leg A), maker gives BTC on Bitcoin (leg B).

**Phase 1: Setup (off-chain, over the Tower relay)**

1. Taker generates a secret `t` and sends the maker `T = t·G`. If the two chains use different curves, a cross-group DLEQ proof that both points share the same `t` goes with it.
2. Both nodes generate fresh per-swap keys and derive the joint locks: `J_A` on leg A, `J_B` on leg B (2-of-2 MuSig2 on Bitcoin, or a PTLC escrow contract on EVM chains).
3. Both co-sign the refunds **before any funding**: `refund_A` (J\_A → taker, valid after `T_long`) and `refund_B` (J\_B → maker, valid after `T_short`). Each owner keeps a fully signed copy.
4. Taker pays the option premium (see Anti-griefing).

**Phase 2: Lock**

5. Taker funds `J_A`.
6. Maker waits for the required confirmations on leg A, then funds `J_B` before the `maker_lock` deadline.

**Phase 3: Pre-signatures**

7. The peers jointly produce an adaptor pre-signature locked to `T` for `claim_B` (J\_B → taker's BTC address). On a signature-only leg A they do the same for `claim_A` (J\_A → maker's USDC address). On a PTLC contract leg, no pre-signature is needed: the contract pays the maker to anyone who submits `t`.
8. Each node verifies every pre-signature, then uploads its watchtower package to the Tower and loads the same package into its own local watcher.

**Phase 4: Settle**

9. Taker completes `claim_B` with `t` and broadcasts it, well before `T_short`. The BTC lands at the taker's address.
10. The maker's node, or any Tower watching, reads the signature from Bitcoin, computes `t = s − s'`, and completes `claim_A` (or submits `t` to the contract) before `T_long`. The USDC lands at the maker's address.

**Abort paths**

- *Maker never locks:* if the maker hasn't funded `J_B`, it can co-sign an instant cancel for `J_A` at no risk to itself. If it's unreachable, the taker broadcasts `refund_A` after `T_long`, and the maker takes a reputation hit.
- *Taker never claims:* the maker refunds at `T_short` and the taker refunds at `T_long`. The maker keeps the premium.
- *Taker tries to claim late:* the node refuses to broadcast `claim_B` after the claim cutoff, which avoids the refund race (see Failure scenarios).

**Timeouts (example for BTC ↔ USDC on Base)**

| Parameter | Meaning | Example |
| --- | --- | --- |
| `maker_lock` | Maker must fund J\_B after leg A confirms | 30 min |
| Claim cutoff | Last moment the taker's node will broadcast claim\_B | T\_short − 1 h |
| `T_short` | Maker's BTC refund unlocks | 4 h |
| `T_long` | Taker's USDC refund unlocks | 12 h |

The gap `T_long − T_short` is the maker's safety window to claim after the secret is revealed. It must cover worst-case confirmation delays on leg A, plus Tower or node downtime.

## Watchtower

The watchtower protects the second mover (the maker), whose claim depends on reacting in time. It does this using only pre-signed transactions that can't be redirected.

**What each node uploads after Phase 3**

| From | Package | Tower action | Trigger |
| --- | --- | --- | --- |
| Maker | `claim_A` pre-signature, or the PTLC contract address and `T` | Extract `t` from `claim_B`, complete and broadcast `claim_A` | `claim_B` seen on leg B |
| Maker | Fully signed `refund_B` | Broadcast it | `T_short` passes with no `claim_B` |
| Taker | Fully signed `refund_A` | Broadcast it | `T_long` passes with `J_A` unspent |

**Why the Tower can't steal:** every package either pays a fixed address of its owner, or needs `t`, which only appears on chain when the taker has already been paid.

**The taker claims for itself.** Giving `t` to a Tower would let a Tower colluding with the maker claim the USDC, withhold `claim_B`, and let the maker refund the BTC after `T_short`. So the taker's node broadcasts `claim_B` itself, right after the maker locks, when it is online anyway. Delegating this to a Tower is allowed only as an explicit opt-in that trusts that Tower.

**Fees**

- *Bitcoin:* pre-signed transactions carry an anchor output, so the Tower or node can bump the fee with CPFP at broadcast time instead of guessing it in advance.
- *EVM:* the PTLC contract lets anyone submit `t`, and pays the submitter a small relayer tip from the locked amount.
- *Tower revenue:* an optional watch fee per swap, paid by the maker. A self-hosted Tower costs nothing.

**Privacy option:** packages can be encrypted under a key derived from the trigger transaction (like Lightning watchtowers). The Tower then learns the details only when the trigger appears on chain.

## Anti-griefing and the free option

Each side has one way to hurt the other. The roles are arranged so that each risk is either paid for or punished.

| Risk | Who is exposed | Defence |
| --- | --- | --- |
| **Free option:** after the maker locks, the taker can wait and claim only if the price moved in its favour | Maker | Taker pays an upfront premium (`premium_bps`), forfeited if it walks. `Keep T_short short (hours, not days), because the option lasts until the claim cutoff` |
| **Lock griefing:** the maker never locks, so the taker's funds sit until `T_long` | Taker | The taker locks only against makers with a track record. A no-show is public on chain and costs the maker its reputation and its listing |
| **Spam takes:** fake takers reserve liquidity and never lock | Maker | Reservation needs a small anti-spam payment (for example a Lightning micro-payment) or proof of a funded wallet. Reservations are short |

**Reputation is verifiable, not trusted.** The Tower already watches both chains, so it can publish per-maker stats (swaps completed, no-shows, median lock time) from on-chain evidence. Any node can recompute them from the same data. Stats live on one Tower, but each completed swap is provable on chain and tied to the maker's node key. So a maker can show its record to a new Tower, which checks it independently.

**Why the taker holds the secret:** retail users lock first and only risk time, never price. Professional makers carry the price option and charge for it through the premium. Reversing the roles would hand the option to the maker and push the price risk onto retail users.

## Maker economics

Makers show up only if spread plus premium beats their costs. The biggest cost is the price option they hand the taker, so the design goal is to shrink that option until it's nearly free.

**What a maker earns and pays per swap**

| Item | Direction | Driven by |
| --- | --- | --- |
| Spread | + | Competition between makers |
| Option premium or forfeited deposit | + | Option window length and volatility |
| Free option given to the taker | − | Time from the maker's lock to the taker's claim |
| Capital locked in J\_B | − | Lock duration × amount; refunds tie it up until `T_short` |
| Hedge cost (CEX fee, slippage) | − | Venue |
| Chain fees and rebalancing inventory across chains | − | Chain choice |
| Griefing losses | − | Anti-spam and reputation |

**How big is the option?** An at-the-money option is worth about 0.4 × σ × √t. At roughly 50% annual BTC volatility, that's about 0.21% of the trade for a 1-hour window and 0.11% for 15 minutes (approximate). That beats a typical CEX fee, so window length matters more than anything else.

**Levers, in order of impact**

1. **Settle fast.** Use Lightning for the BTC leg (submarine-swap style) and an L2 for EVM assets. With seconds-to-minutes windows, the option and the capital lockup become negligible. On-chain BTC stays supported but is priced higher.
2. **Firm quotes through RFQ, not just a static book.** The taker broadcasts "swap 1,000 USDC to BTC", and makers reply within seconds with signed quotes valid for 30–60 s. Makers can hedge on a CEX the moment they are selected, so they carry almost no directional risk. Keep the static book for long-tail pairs.
3. **Premium that honest takers barely notice.** Charge the premium as a small fee folded into the rate, sized to the window (the formula above). For larger trades, consider a refundable deposit locked alongside leg A that goes to the maker only if the taker walks. It's open how to keep a no-show maker from collecting it (see Open questions).
4. **Route flow to good makers.** Rank quotes by price, then by on-chain-verified fill rate and lock speed. Reliable makers win more flow, which is the main reward for uptime.
5. **Charge makers nothing on the Tower.** Makers keep the whole spread. Any Tower fee is a flat watch fee, so it never widens quotes.
6. **Ship an open-source maker bot.** It should handle inventory on both chains, CEX hedging connectors, automatic rebalancing, and risk limits. Running a maker should take an afternoon, not a team.
7. **Bootstrap.** Seed the first pairs with a few committed market makers, which could include the Tower operator, and publish their stats. Without a token, early incentives come from fee rebates or grants, not emissions.

## Chain adapter interface

A chain joins the network once someone writes one adapter for it. The protocol and the Tower never change per chain.

```typescript
interface ChainAdapter {
  curve: "secp256k1" | "ed25519";
  lockKind: "musig2" | "mpc-2of2" | "ptlc-contract";

  // setup
  deriveJointLock(myKey: PubKey, peerKey: PubKey, T: Point,
                  refundTo: Address, refundAfter: Timestamp): Lock;
  buildRefund(lock: Lock): UnsignedTx;          // co-signed before funding
  buildClaim(lock: Lock, payTo: Address): UnsignedTx;

  // adaptor signatures (no-ops for ptlc-contract)
  preSign(tx: UnsignedTx, key: PrivKey, T: Point): PreSig;
  verifyPreSig(tx: UnsignedTx, pre: PreSig, peerKey: PubKey, T: Point): boolean;
  complete(pre: PreSig, t: Scalar): Signature;
  extractSecret(pre: PreSig, onChainTx: Tx): Scalar;   // t = s - s'

  // chain I/O
  fund(lock: Lock, amount: Amount): TxId;
  broadcast(tx: SignedTx, feeBump?: FeeRate): TxId;
  watch(lock: Lock): AsyncIterable<LockEvent>;  // funded | claimed | refunded
  confirmations(txid: TxId): number;
}
```

**Lock kinds by chain family**

| Chain family | Lock kind | Notes |
| --- | --- | --- |
| Bitcoin (Taproot), Litecoin | `musig2` | Key path for claims; refund in a timelocked script leaf |
| EVM (Ethereum, Base, Arbitrum) | `ptlc-contract` | Contract pays on `t` where t·G = T; ecrecover can check this cheaply on secp256k1 |
| Solana, other ed25519 chains | `ptlc-contract` or `mpc-2of2` | Different curve: needs the cross-group DLEQ proof |
| Monero, script-less chains | `mpc-2of2` | No timelocks: refunds rely on key-share reveals, with the timelocks on the other leg |
| Lightning | Hold invoice keyed to `t` | Fastest leg. Uses a SHA-256 hash, so it pairs with a hash-based leg, not a point-based one (see Open questions) |

## Supported chains

Nearly every major chain can do this, but not all can use the same kind of secret. Each swap uses one secret on both legs: a **hash** (SHA-256 preimage, works almost everywhere) or a **point** (`T = t·G` on secp256k1, private and adaptor-friendly). So the node should support both and pick per pair.

One trick makes point locks widely portable: any contract chain that can do secp256k1 public-key recovery (`ecrecover` or an equivalent) can check `t·G = T` cheaply. That lets the same secp256k1 secret work on Ethereum, Solana, Sui, Aptos and others without cross-curve proofs.

| Chain | Hash secret | Point secret | Lock mechanism | Native USDC | Notes |
| --- | --- | --- | --- | --- | --- |
| Bitcoin | Yes | Yes | Taproot: MuSig2 key path + timelocked refund leaf | No | Reference chain; adaptor signatures native |
| Lightning | Yes | No (today) | Hold invoice | No | Fastest BTC leg; hash-only until PTLCs ship |
| Litecoin | Yes | Yes | Taproot, same as Bitcoin | No | Bitcoin code reused almost as-is |
| Bitcoin Cash, Dogecoin, Zcash (transparent), Dash | Yes | Hard | Bitcoin-style script: `OP_SHA256` + CLTV | No | Classic HTLC; no BIP340 Taproot |
| EVM: Ethereum, Base, Arbitrum, OP Mainnet, Polygon, Avalanche, Linea, Unichain, World Chain, Sonic, Monad, HyperEVM, Plasma, Ink, Sei, XDC, Cronos, Morph | Yes | Yes | PTLC/HTLC contract (`ecrecover`) | Yes | Best stablecoin legs; one contract, many chains |
| BNB Smart Chain | Yes | Yes | Same EVM contract | No | USDT and other stablecoins only |
| Hedera | Yes | Yes | EVM-compatible contract | Yes |  |
| Solana | Yes | Yes | Program using the `secp256k1_recover` syscall | Yes | No DLEQ needed when the lock checks secp256k1 |
| Sui | Yes | Yes | Move contract (`ecdsa_k1` recover) | Yes |  |
| Aptos | Yes | Yes | Move contract (`secp256k1` recover) | Yes |  |
| Starknet | Yes | Yes | Cairo contract (secp256k1 syscalls) | Yes |  |
| Stellar | Yes | Yes | Classic: hashX signer + time bounds; Soroban contract for point locks | Yes | Classic HTLC works for any Stellar asset |
| Algorand | Yes | Yes | Smart signature or app (`ecdsa_pk_recover`) | Yes |  |
| NEAR | Yes | Yes | Contract (`env::ecrecover`) | Yes |  |
| Cosmos (CosmWasm), Injective | Yes | Yes | CosmWasm contract (secp256k1 recover) | Injective: yes |  |
| TON | Yes | Likely | Contract (TVM `ECRECOVER`) | No | USDT is the main stablecoin |
| Cardano | Yes | Hard | Plutus validator (SHA-256 builtin) | No | Hash locks only in practice |
| XRP Ledger | Yes | No | Native escrow with a SHA-256 condition and expiry | No | Native escrow works for XRP only |
| Monero | No | Yes (ed25519) | 2-of-2 key shares + cross-curve DLEQ | No | Pairs only with a timelock chain like Bitcoin |

**What this means for pairs**

- **BTC ↔ any USDC chain:** use a point secret with on-chain BTC or a hash secret with Lightning. Every USDC chain in the table supports both.
- **Widest coverage:** hash secrets work on every row except Monero, so they should be the default. Use point secrets when privacy matters or the pair includes Monero.
- **To evaluate later:** Kaspa (Schnorr on secp256k1, new covenant opcodes in progress) and Polkadot Asset Hub (native USDC, contract support still maturing).

The "Native USDC" column follows Circle's CCTP list, plus Hedera, Algorand and NEAR, which Circle lists as native but which aren't on CCTP. Circle has dropped USDC on Tron and announced the end of USDC on Noble. Opcode and syscall names come from each chain's developer docs and should be checked against current versions before building an adapter.

Sources: [Circle CCTP supported chains](https://developers.circle.com/cctp/concepts/supported-chains-and-domains.md) · [Circle on Tron discontinuation](https://www.circle.com/es/blog/circle-is-discontinuing-support-for-usdc-on-the-tron-blockchain) · [Circle native USDC chains](https://link.axios.com/click/29211944.40407/aHR0cHM6Ly93d3cuY2lyY2xlLmNvbS9lbi9wcmVzc3Jvb20vY2lyY2xlLWFubm91bmNlcy1uZXh0LXdhdmUtb2YtdXNkYy1zdXBwb3J0LWZvci1tdWx0aS1jaGFpbi1lY29zeXN0ZW0_dXRtX3NvdXJjZT1uZXdzbGV0dGVyJnV0bV9tZWRpdW09ZW1haWwmdXRtX2NhbXBhaWduPW5ld3NsZXR0ZXJfYXhpb3NjcnlwdG9jdXJyZW5jeSZzdHJlYW09YnVzaW5lc3M/62ff9a0af2eb5f96620a5ad6Bd21a8f3b) · [XRP Ledger escrow](https://xrpl.org/escrow.html) · [Kaspa KIP-17](https://raw.githubusercontent.com/kaspanet/kips/master/kip-0017.md)

## Failure scenarios

In every case below the outcome is either "swap completes" or "everyone is refunded". The one real loss case is the maker missing its claim window, which the Tower and the maker's own always-on watcher together are there to prevent.

| Scenario | What happens | Outcome |
| --- | --- | --- |
| Maker never locks | Instant co-signed cancel, or the taker refunds at `T_long` | Refund; taker loses time, maker loses reputation |
| Taker never claims | Maker refunds at `T_short`, taker at `T_long` | Refund; maker keeps the premium |
| Taker broadcasts `claim_B` near `T_short` (**refund race**) | The maker sees `t` in the mempool and races `refund_B` against `claim_B`. If the refund wins, the maker gets both legs | Prevented: the taker's node refuses to claim after the cutoff |
| Maker goes offline after the taker claims | Tower extracts `t` and broadcasts `claim_A` | Swap completes |
| Maker and Tower both offline until after `T_long` | Taker refunds leg A and already holds leg B | **Maker loses**; mitigated by the maker's own watcher and a wide `T_long − T_short` |
| Tower offline before match | No matching through that Tower | No trade until it's back. Users can move to another Tower, but orders and reputation don't carry over automatically |
| Tower offline mid-swap | Nodes finish over the direct endpoints exchanged at setup; their own watchers replace the watchtower | Swap completes or refunds normally |
| Fee spike on leg B | Claims and refunds are bumped through anchor outputs (CPFP) | Completes if margins hold |
| Chain reorg on leg A before the maker locks | Maker waits for N confirmations per chain | Covered by the confirmation policy |
| Malicious Tower leaks swap metadata | Privacy loss only | Tor, fresh keys, self-hosting |

## Deployment model, MVP and open questions

**Deployment model: one Tower per market, no federation.** The code is open source and anyone can deploy a Tower, but every deployment runs in isolation. Decentralization comes from the fact that anyone can deploy a Tower and no Tower ever holds funds, not from Towers coordinating with each other.

- **Isolated:** each Tower has its own order book, relay, watchtower and reputation data. There's no gossip, no shared book, and no cross-Tower matching.
- **One Tower per node session:** a node is configured with a Tower URL plus the Tower's public key, which it pins. Both swap peers must be on the same Tower.
- **Liquidity spreads through makers, not Towers:** a maker can run its bot against several Towers independently. The bot must cap quoted inventory across them, so a fill on one Tower can't overcommit funds quoted on another.
- **No backup Tower, so the node is the backup:** every node runs a local watcher with the same pre-signed packages it gives the Tower. The Tower is a convenience; the node is the guarantee.
- **Trust is per operator:** users pick a Tower operator they trust for uptime and privacy. Funds are safe whichever operator they pick.
- **Trade-offs:** simpler to run, no inter-Tower consensus, and no spam crossing between Towers. The cost is fragmented liquidity, and each market has a single point of failure for liveness, though not for funds.
- **Transparency:** each Tower publishes its own append-only order and match log, so users can audit that Tower for censorship or front-running.

**MVP scope**

1. Pair: BTC on-chain (Taproot + MuSig2) ↔ USDC on Base (PTLC contract).
2. Node daemon: keys, swap state machine, chain watching, CLI and API.
3. Tower: order book and RFQ, Noise relay, watchtower, reputation stats.
4. PTLC escrow contract, audited.
5. Reference maker bot: inventory on both sides, one CEX hedge connector.
6. Later: a Lightning leg, more EVM chains, Solana (DLEQ), Monero.

**Open questions**

- [ ] Refundable deposit: how to pay it to the maker on a taker no-show, without paying a maker who never locked?
- [ ] Lightning pairs: Lightning uses SHA-256 hashlocks today, so the other leg would use a hash-based HTLC instead of a point lock. Proposed: support both secret types and pick per pair (see Supported chains).
- [ ] Taker online time: the taker must stay online from its lock until its claim. Is the target under 15 minutes on fast chains?
- [ ] USDC issuer risk: Circle can freeze the escrow contract. Is that acceptable for the MVP?
- [ ] Legal: does running a public matching Tower count as operating an exchange in target jurisdictions? Needs counsel; this doc isn't legal advice.
- [ ] DoS: rate limits and anti-spam payments for order posting and reservations.
