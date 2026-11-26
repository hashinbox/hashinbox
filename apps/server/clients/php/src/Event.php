<?php

declare(strict_types=1);

namespace Hashinbox;

/**
 * Hand-written: one event of a stream, its data decoded.
 *
 * @template-covariant T
 */
final readonly class Event
{
    /**
     * @param T $data
     */
    public function __construct(public string $event, public mixed $data, public ?string $id = null) {}
}
