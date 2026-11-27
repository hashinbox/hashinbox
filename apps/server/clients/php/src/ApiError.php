<?php

declare(strict_types=1);

namespace Hashinbox;

use JsonException;
use RuntimeException;
use Stringable;
use Throwable;

/**
 * Hand-written: any failure as the API's error: its kind, message
 * (`getMessage()`), reason (what a client acts on, one of `Reason`'s),
 * metadata, and the answer's status (0 when none came). A plain value;
 * `from` turns anything into one.
 */
final class ApiError extends RuntimeException
{
    /**
     * @param array<string, string> $metadata
     */
    public function __construct(
        string $message,
        public readonly Kind $kind = Kind::Unavailable,
        public readonly ?string $reason = null,
        public readonly array $metadata = [],
        public readonly int $status = 0,
        ?Throwable $previous = null,
    ) {
        parent::__construct($message, 0, $previous);
    }

    /**
     * `$x` as an API error: itself when it is one; an answer in the API's
     * error shape (a kind it knows, a message) that error, with its status;
     * any other answer (HTML, empty, other JSON, as `healthGet`'s 503
     * `Report`) `Unavailable`, its status, its body's first 1024 characters
     * as `metadata['response']`; another throwable `Unavailable`, its
     * message, itself as previous; anything else `Unavailable`, as a string.
     */
    public static function from(mixed $x): self
    {
        if ($x instanceof self) {
            return $x;
        }
        if ($x instanceof Reply) {
            return self::answer($x->status, $x->body());
        }
        if ($x instanceof Throwable) {
            return new self($x->getMessage(), previous: $x);
        }
        return new self(is_scalar($x) || $x instanceof Stringable ? (string) $x : get_debug_type($x));
    }

    /**
     * The error a `$status` answer with body `$text` is.
     */
    private static function answer(int $status, string $text): self
    {
        try {
            $body = Json::decode($text);
        } catch (JsonException) {
            $body = null;
        }
        $error = is_array($body) ? $body : [];
        $kind = is_string($error['kind'] ?? null) ? Kind::tryFrom($error['kind']) : null;
        $message = $error['message'] ?? null;
        if ($kind === null || !is_string($message)) {
            return new self(
                "could not parse the response (HTTP {$status})",
                metadata: ['response' => mb_substr($text, 0, 1024)],
                status: $status,
            );
        }
        $reason = $error['reason'] ?? null;
        $metadata = [];
        foreach (is_array($error['metadata'] ?? null) ? $error['metadata'] : [] as $key => $value) {
            if (is_string($value)) {
                $metadata[(string) $key] = $value;
            }
        }
        return new self(
            $message,
            kind: $kind,
            reason: is_string($reason) ? $reason : null,
            metadata: $metadata,
            status: $status,
        );
    }
}
