<?php

declare(strict_types=1);

namespace Hashinbox\Tests;

use Generator;
use Hashinbox\ApiError;
use Hashinbox\Kind;
use Hashinbox\Reason;
use Hashinbox\Reply;
use LogicException;
use PHPUnit\Framework\Attributes\DataProvider;
use PHPUnit\Framework\TestCase;
use RuntimeException;

final class ApiErrorTest extends TestCase
{
    public function testAnApiErrorIsItself(): void
    {
        $error = new ApiError('code', Kind::Unauthenticated, Reason::TotpRequired, status: 401);
        self::assertSame($error, ApiError::from($error));
        self::assertTrue(ApiError::from($error)->reason === Reason::TotpRequired);
    }

    public function testAMadeErrorIsUnavailableUnlessSet(): void
    {
        $error = new ApiError('made');
        self::assertSame([0, Kind::Unavailable, null, []], [$error->status, $error->kind, $error->reason, $error->metadata]);
    }

    public function testATransportFailureIsUnavailable(): void
    {
        $failure = new RuntimeException('curl: refused');
        $error = ApiError::from($failure);
        self::assertSame([0, Kind::Unavailable, null, 'curl: refused'], [$error->status, $error->kind, $error->reason, $error->getMessage()]);
        self::assertSame($failure, $error->getPrevious());
    }

    public function testAnythingElseIsUnavailable(): void
    {
        $failure = new LogicException('boom');
        $error = ApiError::from($failure);
        self::assertSame([0, Kind::Unavailable, null, 'boom'], [$error->status, $error->kind, $error->reason, $error->getMessage()]);
        self::assertSame($failure, $error->getPrevious());
        $odd = ApiError::from('odd');
        self::assertSame([0, Kind::Unavailable, null, 'odd'], [$odd->status, $odd->kind, $odd->reason, $odd->getMessage()]);
    }

    /**
     * @return iterable<string, array{int, string}>
     */
    public static function unparsed(): iterable
    {
        yield 'html 502' => [502, '<html>' . str_repeat('x', 2000) . '</html>'];
        yield 'empty 503' => [503, ''];
        yield 'another JSON shape' => [503, '{"status":"down"}'];
        yield 'an unknown kind' => [400, '{"kind":"nope","message":"x"}'];
    }

    #[DataProvider('unparsed')]
    public function testAnAnswerNotAnErrorIsUnavailable(int $status, string $text): void
    {
        $error = ApiError::from(self::reply($status, $text));
        self::assertSame(
            [$status, Kind::Unavailable, null, "could not parse the response (HTTP {$status})", ['response' => substr($text, 0, 1024)]],
            [$error->status, $error->kind, $error->reason, $error->getMessage(), $error->metadata],
        );
    }

    public function testAnErrorBodyIsThatError(): void
    {
        $error = ApiError::from(self::reply(401, '{"kind":"unauthenticated","message":"code","reason":"totp_required","metadata":{"a":"b"}}'));
        self::assertSame(
            [401, Kind::Unauthenticated, 'code', Reason::TotpRequired, ['a' => 'b']],
            [$error->status, $error->kind, $error->getMessage(), $error->reason, $error->metadata],
        );
    }

    private static function reply(int $status, string $text): Reply
    {
        $chunks = (static function () use ($text): Generator {
            yield $text;
        })();
        return new Reply($status, $chunks);
    }
}
