<?php

declare(strict_types=1);

namespace Hashinbox\Tests;

use Closure;
use Generator;
use Hashinbox\Reply;
use Hashinbox\Transport;

/**
 * A transport answering from a closure, keeping what was sent.
 *
 * @phpstan-type Sent array{method: string, url: string, headers: array<string, string>, body: ?string, stream: bool}
 */
final class FakeTransport implements Transport
{
    /** @var list<Sent> */
    public array $sent = [];

    /**
     * @param Closure(Sent): array{int, string|list<string>} $answer the status, and the body or its chunks
     */
    public function __construct(private readonly Closure $answer) {}

    public function open(string $method, string $url, array $headers, ?string $body, float $timeout, bool $stream): Reply
    {
        $request = ['method' => $method, 'url' => $url, 'headers' => $headers, 'body' => $body, 'stream' => $stream];
        $this->sent[] = $request;
        [$status, $answer] = ($this->answer)($request);
        $chunks = is_string($answer) ? [$answer] : $answer;
        return new Reply($status, (static function () use ($chunks): Generator {
            yield from $chunks;
        })());
    }
}
