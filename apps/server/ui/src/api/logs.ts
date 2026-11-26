// `/v1/logs` and its stream, as the log view (`@/_components/logs`) reads them.
import { logMessagesList, logsList, logsStream, logTargetsList } from '@hashinbox/client';
import { api } from '@/api/client';

/** A filter as query text: `level`, `target`, `network`, `job_id`, `before`, `after`… */
type Query = Record<string, string>;

/** One server-sent event: its name and its JSON data. */
interface StreamEvent {
  event: string;
  data: unknown;
}

/** The message templates, read once a session (again after a failed read). */
let messages: Promise<Record<string, string>> | undefined;

export const logSource = {
  read: (query: Query, cursor: string | undefined) =>
    logsList(api, { ...query, ...(cursor !== undefined && { cursor }) }),
  /**
   * Hands `onEvent` each entry of the stream, window after window (the
   * handle resumes after the last id: none missed, none twice); `signal`
   * stops it.
   */
  listen: async (query: Query, onEvent: (event: StreamEvent) => void, signal: AbortSignal) => {
    const stream = logsStream(api, query);
    signal.addEventListener('abort', () => stream.stop(), { once: true });
    if (signal.aborted) stream.stop();
    for await (const { event, data } of stream)
      onEvent({ event, data: JSON.parse(data) as unknown });
  },
  targets: () => logTargetsList(api).then((list) => list.targets),
  messages: () =>
    (messages ??= logMessagesList(api).then(
      (list) => list.messages,
      (error: unknown) => {
        messages = undefined;
        throw error;
      },
    )),
};
