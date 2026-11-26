<?php

declare(strict_types=1);

namespace Hashinbox;

use JsonException;
use RuntimeException;

/**
 * Hand-written: a non-2xx answer: its status, the API error body's fields
 * (`getMessage()` is its message), and the body as sent (`text`; `body` when
 * JSON, maybe another model, as `healthGet`'s 503 `Report`). `reason` is what
 * a client acts on: `wrong_credentials`, `totp_required`, `session_expired`,
 * `missing_scope`.
 */
final class ApiError extends RuntimeException
{
    /**
     * @param array<string, string> $metadata
     */
    public function __construct(
        public readonly int $status,
        public readonly ?Kind $kind,
        string $message,
        public readonly ?string $reason = null,
        public readonly array $metadata = [],
        public readonly string $text = '',
        public readonly mixed $body = null,
    ) {
        parent::__construct($message);
    }

    /**
     * The error a `$status` answer with body `$text` is.
     */
    public static function read(int $status, string $text): self
    {
        try {
            $body = Json::decode($text);
        } catch (JsonException) {
            $body = null;
        }
        $error = is_array($body) ? $body : [];
        $kind = is_string($error['kind'] ?? null) ? Kind::tryFrom($error['kind']) : null;
        $message = $error['message'] ?? null;
        $reason = $error['reason'] ?? null;
        $metadata = [];
        foreach (is_array($error['metadata'] ?? null) ? $error['metadata'] : [] as $key => $value) {
            if (is_string($value)) {
                $metadata[(string) $key] = $value;
            }
        }
        return new self(
            status: $status,
            kind: $kind,
            message: is_string($message) ? $message : "HTTP {$status}",
            reason: is_string($reason) ? $reason : null,
            metadata: $metadata,
            text: $text,
            body: $body,
        );
    }
}
