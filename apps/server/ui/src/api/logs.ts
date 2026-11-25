// `/v1/logs` and its stream, as the log view (`@/_components/logs`) reads them.
import { call, client, events, type StreamEvent } from '@/api/client';

/** A filter as query text: `level`, `target`, `network`, `job_id`, `before`, `after`… */
type Query = Record<string, string>;

/** The message templates, read once a session (again after a failed read). */
let messages: Promise<Record<string, string>> | undefined;

export const logSource = {
  read: (query: Query, cursor: string | undefined) =>
    call(
      client.GET('/v1/logs', {
        params: { query: { ...query, ...(cursor !== undefined && { cursor }) } },
      }),
    ),
  listen: (query: Query, onEvent: (event: StreamEvent) => void, signal: AbortSignal) =>
    events(`/v1/logs/stream?${new URLSearchParams(query).toString()}`, onEvent, signal),
  targets: () => call(client.GET('/v1/logs/targets')).then((list) => list.targets),
  messages: () =>
    (messages ??= call(client.GET('/v1/logs/messages')).then(
      (list) => list.messages,
      (error: unknown) => {
        messages = undefined;
        throw error;
      },
    )),
};
