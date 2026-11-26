# 099: Tron

Status: implemented (`sdk/chains/tron`, crate `chain-tron`). Migration: `099-migrate-1.sql`.

## Scope

- Native TRX and allowlisted TRC-20 tokens: USDT on mainnet (`TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t`)
  and Nile's test USDT (`TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf`); both 6 decimals, symbol `USDT`,
  read from the contracts (`decimals()`, `symbol()`).
- No USDC: Circle stopped minting USDC on Tron in February 2024 and ended its support; about
  28M stay on chain (`TEkxiTehnzSmSe2XqrBj4w32RUN966rdz8`), not redeemable. A network may still
  allowlist it as any token.
- Networks: `mainnet` and `nile` (Shasta is older and less used). Identity (`chain_id`): the
  genesis block id.

## Transport

- The node's HTTP API (java-tron, TronGrid): REST, `POST /wallet/<call>` with a JSON body.
  `rpc::Http` treats a method that is a path as REST: params are the body, the body the answer;
  `{"Error": …}` or `result: false` is the node's error object (code `-32000`, `"<code>: <message>"`).
  Same decorators, retry, fan-out and node selection as JSON-RPC.
- Not the `/jsonrpc` endpoint: it cannot broadcast (no `eth_sendRawTransaction`) and its
  receipts give energy, not the TRX burned.
- Nodes: mainnet `tron-rpc.publicnode.com` then `api.trongrid.io`; Nile `nile.trongrid.io`.
  TronGrid without a key answers HTTP 429 past a few heavy calls a second (mainnet blocks and
  their infos are heavy): add `?header.TRON-PRO-API-KEY=<key>`.

## Reading

- Tip: `/wallet/getblock {"detail": false}` (header only). Time: the same by number.
- A block: `/wallet/getblockbynum`, then, when it has transactions,
  `/wallet/gettransactioninfobyblocknum` (one info per transaction, same order, ids checked).
- Legs, of the contract's owner:
  - `TransferContract`: `amount` spent and received; a `TriggerSmartContract`'s `call_value` too;
  - TRC-20: each `Transfer` log (`ddf252ad…`) of an allowlisted contract: spent/received, mint
    from zero, burn to zero;
  - fee: the info's `fee`, all TRX burned (bandwidth, energy, an account created: 1.1 TRX).
  - A failed transaction (`result: FAILED`, e.g. `OUT_OF_ENERGY`): reverted, its fee only.
- `{}` for a block not produced: the run ends. No infos for a block near the tip: the run ends
  before it; 19 or more below the tip: `NotServed`.

## Finality

- Confirmed: 19 below the tip (`confirmations`), about where the solidified block sits
  (`/walletsolidity/getblock`; 2/3 of 27 SRs build on it). `base` computes confirmed as
  `tip − confirmations`, so the solidified block is not read for it.
- Line: `Depth(max_depth)` (seeded `NULL`: none). Tron's API reads no balance or call at a past
  block, so there is no `Balances` (an index is read block by block, never settled) and no
  `Confirmer`, as Bitcoin.

## Sending

- `raw_data` is protobuf, encoded here (`TransferContract` type 1, `TriggerSmartContract` 31,
  `Any`, `Transaction.raw`), never taken from a node; checked byte for byte against Nile's own
  `raw_data_hex`. Id: SHA-256 of `raw_data`. Signature: secp256k1 over the id, `r ‖ s ‖ 27+v`.
  Broadcast: `/wallet/broadcasthex` of the signed `Transaction`; `DUP_TRANSACTION_ERROR` is
  acceptance, `TRANSACTION_EXPIRATION_ERROR` expired, `SIGERROR`/`TAPOS_ERROR` invalid,
  `BANDWITH_ERROR` and an unfunded owner insufficient funds.
- Reference (TaPoS): the solidified block, so a transaction is valid on every fork. Expiration:
  its time + 3 min (about 2 min after signing); `expires_height` = reference + 60 (blocks are at
  least 3 s apart). The timestamp is the signing millisecond, unique in the process, so two
  equal payments never share an id.
- No nonce: like Solana, a payment is never replaced; once expired unseen, the broadcast job
  signs a new version (rate a tenth up). Tracking: every 10 s, at most 5 versions.

## Fees

- Prices from `/wallet/getchainparameters` (kept `fee_ttl_ms`, 60 s): energy (`getEnergyFee`,
  100 sun), bandwidth (`getTransactionFee`, 1,000 sun/byte), new account
  (`getCreateAccountFee` + `getCreateNewAccountFeeInSystemContract`, 1.1 TRX).
- Rate: `base` = energy price, `tip` 0 (no market).
- A token transfer's `fee_limit` = `limits.energy` (200,000; USDT to a new holder uses ~130,000
  at the top dynamic-energy factor) × the rate's base; above `limits.max_fee_limit` (100 TRX):
  `FeeCap`.
- Worst-case fee leg (= estimate): TRX 300 bytes × bandwidth, plus the new-account fee unless
  `Holdings` say the recipient holds TRX; a token 400 bytes × bandwidth + `fee_limit`. The leg
  once mined is the TRX actually burned.

## Not supported

- Staking (freeze/unfreeze, v1 and v2), resource delegation, voting, rewards.
- TRC-10 (`TransferAssetContract`) and TRX moved inside contracts (internal transactions).
- Multisig (permission ids other than owner), memos, account activation without a transfer.
- Balance reads and settles at a past block; energy estimation per payment.
