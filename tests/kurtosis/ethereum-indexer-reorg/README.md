# Opt-in Ethereum reorg validation against a real fork

This scenario validates `payment-server`'s Ethereum indexing against a real
pre-finality Ethereum fork. It is manual and opt-in: nothing here runs under
`cargo test`, and this directory holds only the pinned network definition
([`network_params.yaml`](./network_params.yaml)) plus this runbook.

The network isolates participant 1 from participants 2–4 at both the execution
and the consensus P2P layer. Point one `payment-server` at participant 1, wait
for a watched address's transaction to be included on that minority branch,
then heal the partition. The majority branch wins, and the server must discard
the orphaned inclusion, replay the replacement branch, and converge — over plain
JSON-RPC polling, which is the only transport the Ethereum chain crate uses.

The fork this scenario causes is **pre-finality by construction**, which is
exactly the region the indexer keeps in memory: Ethereum's confirmed line is the
node's own `finalized` block, the server commits nothing above it, and the whole
reorg therefore happens in the pending blocks without touching a stored row.
A fork that reached a *finalized* block would be a consensus
failure, not a reorg, and the server would quarantine that network with a
terminal `confirmed_reorg`.

Exact fork depth is deliberately not asserted. Proposer assignment and timing
make a Disruptoor fork nondeterministic; the deterministic Rust doubles in
`sdk/chains/ethereum` and `sdk/indexing` own the exact depth cases, including a
reorg below the confirmed line.

## Pins

- `ethereum-package`: `8e81c5709deffad01d0bd1770d259c224f1ed349`
- Geth: `ethereum/client-go:v1.17.3`, multi-platform digest
  `sha256:ee8af3a081fd8f780509541cf8115be03259e106375e50efa9d995400f86e15d`
- Lighthouse: `sigp/lighthouse:v8.1.3`, multi-platform digest
  `sha256:44aa773dcf274122c32cc41c4dae7124ed3cab5c267ee82174c6dfd9c688da96`
- Disruptoor and Dora are also pinned by multi-platform digest in
  [`network_params.yaml`](./network_params.yaml).

Review and deliberately update all pins together. Do not replace them with
`latest`.

## Prerequisites

- Docker backend, not Kubernetes;
- a current Kurtosis CLI and engine supporting `--privileged`;
- permission to mount the Docker socket and use the host PID namespace;
- enough local CPU, memory, and disk for four Geth/Lighthouse pairs; and
- a built `payment-server` plus a fresh, disposable index path. Never point
  this at an index file that holds anything else.

Disruptoor needs privileged containers, `/var/run/docker.sock`, and host PID
access. Never run this profile on an untrusted shared Docker host.

## Start the network

Start the host-level OTel stack first:

```bash
kurtosis otel start
```

Then run the pinned package from the repository root:

```bash
kurtosis run \
  --enclave payment-sdk-ethereum-reorg \
  github.com/ethpandaops/ethereum-package@8e81c5709deffad01d0bd1770d259c224f1ed349 \
  --args-file tests/kurtosis/ethereum-indexer-reorg/network_params.yaml \
  --image-download always \
  --privileged \
  --verbosity detailed
```

Inspect the enclave and record participant 1's published Geth HTTP endpoint,
the majority participant endpoints, Dora, and Disruptoor:

```bash
kurtosis enclave inspect payment-sdk-ethereum-reorg
```

Do not put RPC credentials, bearer tokens, or private keys in this directory or
in captured output.

## Exercise the server

1. Write a configuration whose single `chains.ethereum.rpc.endpoints` entry is
   participant 1's Geth HTTP endpoint and the enclave's chain ID. Ethereum takes
   no confirmation depth: the node states its own `finalized` block and the
   server commits there. Storage is a fresh index file. Start the server
   using `apps/server/README.md`, including the wallet passphrase, and wait for
   `GET /health` to answer `200`.
2. Watch an address with `POST /v1/wallets` and an `address`, and record the
   returned wallet ID. Note the checkpoint in `GET /health` before sending anything.
3. Use the enclave's disposable prefunded test account to send that address a
   transfer through participant 1. This is devnet-only test value; never reuse
   a real key.
4. Poll `GET /v1/wallets/{id}/transactions` until the transaction appears.
   Record its `transaction_id`, its `status.kind`, and its `status.block`
   (`position`, `height`, `hash`).
5. Confirm through Dora, or through both branches' RPC endpoints, that the
   partition has produced different block hashes at that height. If participant
   1 has not included the test transaction, keep the partition active rather
   than weakening the assertions below.
6. Heal `isolate-indexer-branch` through the Disruptoor control surface the
   enclave exposes. Confirm peer connectivity and wait for participant 1 to
   adopt the majority canonical branch.
7. Allow at least one polling interval (`chains.ethereum.cadence.poll_millis`).

## Required assertions

- The pre-heal inclusion hash recorded in step 4 is no longer canonical at its
  height on participant 1.
- `GET /health` shows `checkpoint` moving **forward only** across the whole
  scenario: the fork is above the confirmed line, so no committed block is ever
  taken back. `pending_block` follows the branch that wins.
- `GET /v1/wallets/{id}/transactions` no longer lists the orphaned inclusion in
  `pending`. Either the transaction reappears at its replacement block, or it is
  absent — history holds canonical transactions only, and there is no
  tombstone, revision, or event feed.
- A history cursor taken before the fork still answers, because confirmed
  history is append-only and the fork never reached it. A `409` here would be a
  defect.
- The server survives a restart mid-scenario: it resumes from the persisted
  checkpoint and reaches the same canonical history.
- No `eth_pendingTransactions`, pending subscription, txpool, trace, `safe`, or
  `finalized` RPC is issued.
- Stopping participant 1 pauses progress and is reported as retrying
  (`GET /health/ready` answers `503`); it never produces a failed or dropped
  transaction by inference.

Keep the enclave on failure so logs and OTel data stay inspectable. Remove it
explicitly after collecting evidence:

```bash
kurtosis enclave rm -f payment-sdk-ethereum-reorg
```

The host-level OTel stack is shared with other enclaves; stop it only when no
other work depends on it.
