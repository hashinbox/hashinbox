<?php

declare(strict_types=1);

namespace Hashinbox;

/**
 * Hand-written: one server-sent event, as read.
 */
final readonly class RawEvent
{
    public function __construct(public string $event, public string $data, public ?string $id = null) {}
}
