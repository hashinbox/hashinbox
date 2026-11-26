<?php

declare(strict_types=1);

namespace Hashinbox;

use Closure;
use Generator;
use IteratorAggregate;
use RuntimeException;

/**
 * Hand-written: a stream's handle (`Client::open`, every generated stream
 * operation). The server ends each answer after its window (15 s) with
 * `event: close`; the handle then reconnects at once, sending the last event
 * `id` it read as the query's `cursor`, so its events go on across windows
 * (the first answer is sent with the request's own `cursor`, if any). It
 * reconnects after a backoff (doubling, capped) when the transport fails, the
 * body is cut or the server answers 5xx or 429; any other refusal is thrown
 * (`ApiError`). An answer that ends without `close` is the stream's end.
 *
 * PHP blocks: the events are read as they are iterated, and `stop()`, called
 * from the loop's body, is checked after each event and before each
 * reconnect. Without it the handle reconnects every window, for ever:
 *
 *     $logs = logsStream($api);
 *     foreach ($logs as $event) {
 *         handle($event);
 *         if ($enough) {
 *             $logs->stop();
 *         }
 *     }
 *     $resumeFrom = $logs->cursor();
 *
 * Breaking out of the loop ends it too; iterating again goes on from the
 * cursor.
 *
 * @template-covariant T
 * @implements IteratorAggregate<int, T>
 */
final class Stream implements IteratorAggregate
{
    private bool $stopped = false;

    /**
     * @param Closure(?string): Reply $connect one answer, from the cursor given (null: the request's own); its refusal thrown
     * @param Closure(RawEvent): (T|null) $decode an event as handed out; null: skipped
     * @param float $backoff seconds before the first reconnect after an error, doubled each time
     * @param float $backoffMax the most seconds between reconnects
     */
    public function __construct(
        private readonly Closure $connect,
        private readonly Closure $decode,
        private readonly float $backoff = 0.5,
        private readonly float $backoffMax = 8.0,
        private ?string $cursor = null,
    ) {}

    /**
     * The `id` of the last event read (every window's, `close` too): the
     * cursor a reconnect sends; null before one.
     */
    public function cursor(): ?string
    {
        return $this->cursor;
    }

    /**
     * Ends the stream: no event after the current one, no reconnect.
     */
    public function stop(): void
    {
        $this->stopped = true;
    }

    /**
     * @return Generator<int, T, mixed, void>
     *
     * @throws ApiError a refusal other than 5xx or 429
     * @throws MissingScope a scope the session lacks, unsent
     */
    public function getIterator(): Generator
    {
        $delay = $this->backoff;
        while (!$this->stopped) {
            try {
                $reply = ($this->connect)($this->cursor);
                $delay = $this->backoff;
                $closed = false;
                foreach (self::events($reply->chunks()) as $raw) {
                    $this->cursor = $raw->id ?? $this->cursor;
                    if ($raw->event === 'close') {
                        $closed = true;
                        break;
                    }
                    $event = ($this->decode)($raw);
                    if ($event !== null) {
                        yield $event;
                    }
                    if ($this->stopped) {
                        return;
                    }
                }
                if (!$closed) {
                    return;
                }
                continue;
            } catch (ApiError $error) {
                if ($error->status < 500 && $error->status !== 429) {
                    throw $error;
                }
            } catch (MissingScope $error) {
                throw $error;
            } catch (RuntimeException) {
                // The transport failed, or the body was cut: try again.
            }
            usleep((int) ($delay * 1_000_000));
            $delay = min($delay * 2, $this->backoffMax);
        }
    }

    /**
     * The events of a `text/event-stream` body, chunk by chunk; each carries
     * the last id sent so far.
     *
     * @param iterable<string> $chunks
     * @return Generator<int, RawEvent, mixed, void>
     */
    private static function events(iterable $chunks): Generator
    {
        $event = null;
        $id = null;
        $data = [];
        $buffer = '';
        foreach ($chunks as $chunk) {
            $buffer .= $chunk;
            while (($end = strpos($buffer, "\n")) !== false) {
                $line = rtrim(substr($buffer, 0, $end), "\r");
                $buffer = substr($buffer, $end + 1);
                if ($line === '') {
                    if ($data !== []) {
                        yield new RawEvent($event ?? 'message', implode("\n", $data), $id);
                    }
                    [$event, $data] = [null, []];
                    continue;
                }
                [$name, $value] = array_pad(explode(':', $line, 2), 2, '');
                $value = str_starts_with($value, ' ') ? substr($value, 1) : $value;
                match ($name) {
                    'event' => $event = $value === '' ? null : $value,
                    'data' => $data[] = $value,
                    'id' => $id = $value === '' ? null : $value,
                    default => null,
                };
            }
        }
    }
}
