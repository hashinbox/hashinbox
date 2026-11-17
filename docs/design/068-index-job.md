# 068: Index job

Status: implemented.

Every piece of the server's background work is a job of one executor:

```
index (per network, forever) -> triggers (per round) -> broadcast (per payment)
```

There are no loops, handles or channels outside the executor any more.

## Jobs

| Job | Topic | Lives | Does |
| --- | --- | --- | --- |
| `index` | `index:chain:network` | forever, id `index.chain.network` | steps the indexer; at the tip queues `triggers` |
| `triggers` | `chain:network` | one round | every enabled trigger decides; actions start jobs |
| `broadcast` | `chain:network.sender` | one payment, until final or dead | sign, check and record, broadcast one transaction; check it every minute: done once final, forgotten if dead, else sent again (066, 084) |

`index` conflicts with nothing: each of its steps is one transaction, so it
runs beside triggers and broadcasts. A forever job may not share a topic
with others: it is always unfinished, and an earlier unfinished job blocks
every later one it conflicts with (067). A broadcast job waiting for its
next check blocks nobody until it is due.

At startup `main` makes sure each network has its `index` job (a missing
one is enqueued, a finished one starts over).

## `tasks`: `Ack::Again(at)`

A forever job answers `Ack::Again(at)` (`Task::again`): pending again at `at`
(Unix ms), attempts and reason cleared, state kept. One row per job, never
pruned.

## The index job

Up to 10 steps of `Indexer::next` (100 blocks each) per run:

- `Idle` (at the tip): queue a `triggers` job; run again one
  `interval_ms` later (about a block time, per chain config).
- still behind (a backfill, a restart after downtime): run again at once.
- a failed step (counted in the indexer's stats): run again one interval
  later. The job never fails.

Triggers are queued only at the tip, never while catching up.

## Triggers

- A round has no block list: every enabled trigger decides, every interval.
- `Triggers::new` reads a network's enabled triggers at its last kept block;
  `decide` runs each `Trigger` (one entity: its row, the round's `Context`,
  its last run, and the evaluation: select, filter, check, action). An action
  gets an `Invocation` (the trigger, `This`, the keyed addresses). The worker
  is `Round`.
- No contested wallets: a wallet two triggers pick is seen by both. All decide
  on one snapshot; if both send it, the later broadcast fails its funds check
  (`insufficient-funds`), visibly.
- Balances are **spendable**: confirmed, less what pending transactions take
  (spent, burned, fee). What they bring counts once mined. A trigger that sent
  sees the funds gone at once, so it does not send them again; a send
  proven dead gives them back (084).
- Runs are only a record (`started_at`, the jobs). `settled_at`, run settling
  and the `idle` gate are gone; `since` still reads the last run's start.

## `base`

- `Indexer` keeps `new(chain, blocks, indexes, interval)`, `next`, `watch`,
  `stats`, `interval`. `next` counts its own failures.
- Gone: `Indexer::run`, `drain`, `Handle`, `Handles`, `Indexers`, `Notified`,
  `Timer`, and the indexer's logger.
- `Notifying` (a block store that tells listeners after each insert) stays for
  library users; the server does not use it.
- `payments::Payments::run` is gone.

A fresh database is needed (`trigger_run.settled_at` is dropped).
