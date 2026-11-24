// Opens the logs drawer from anywhere: the one drawer, mounted once, listens.
import type { LogFilter } from './filter';

let show: ((filter: LogFilter) => void) | undefined;

/** Opens the logs drawer on `filter`: `{ job_id }`, `{ network }`, `{ wallet_id }`… */
export function openLogs(filter: LogFilter): void {
  show?.(filter);
}

/** Who opens the drawer; `undefined` when it is gone. */
export function onOpenLogs(handler: ((filter: LogFilter) => void) | undefined): void {
  show = handler;
}
