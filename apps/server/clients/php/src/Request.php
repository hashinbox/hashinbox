<?php

declare(strict_types=1);

namespace Hashinbox;

/**
 * Hand-written: an operation's input, as each generated request class gives it.
 */
interface Request
{
    public function operation(): Operation;

    /**
     * The path parameters by name.
     *
     * @return array<string, string>
     */
    public function params(): array;

    /**
     * The query pairs, in order.
     *
     * @return list<array{string, string}>
     */
    public function query(): array;

    /**
     * The JSON body; null when none.
     */
    public function content(): ?string;
}
