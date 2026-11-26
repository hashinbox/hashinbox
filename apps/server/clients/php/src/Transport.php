<?php

declare(strict_types=1);

namespace Hashinbox;

use RuntimeException;

/**
 * Hand-written: how `Client` sends one request: `CurlTransport` by default, a
 * fake in tests.
 *
 * @internal
 */
interface Transport
{
    /**
     * Sends `$method $url`: its answer once the status is in, its body read as
     * it arrives. `$timeout` bounds the whole call, or only connecting when
     * `$stream`.
     *
     * @param array<string, string> $headers
     * @throws RuntimeException the request could not be sent or answered
     */
    public function open(string $method, string $url, array $headers, ?string $body, float $timeout, bool $stream): Reply;
}
