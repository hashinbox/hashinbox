<?php

declare(strict_types=1);

namespace Hashinbox;

use Generator;

/**
 * Hand-written: a `Transport`'s answer: its status, and its body chunk by
 * chunk (read once).
 *
 * @internal
 */
final class Reply
{
    /**
     * @param Generator<int, string, mixed, void> $chunks
     */
    public function __construct(public readonly int $status, private readonly Generator $chunks) {}

    public function ok(): bool
    {
        return $this->status >= 200 && $this->status < 300;
    }

    /**
     * @return Generator<int, string, mixed, void>
     */
    public function chunks(): Generator
    {
        return $this->chunks;
    }

    /**
     * The rest of the body.
     */
    public function body(): string
    {
        $body = '';
        foreach ($this->chunks as $chunk) {
            $body .= $chunk;
        }
        return $body;
    }
}
