<?php

declare(strict_types=1);

namespace Hashinbox\Tests;

use Hashinbox\Client;
use Hashinbox\Secret;
use PHPUnit\Framework\TestCase;

use function Hashinbox\healthGet;
use function Hashinbox\sessionGet;

/**
 * Against a running server: `HASHINBOX_URL`, and `HASHINBOX_KEY_ID` with
 * `HASHINBOX_KEY_SECRET` for the calls a key makes.
 */
final class LiveTest extends TestCase
{
    private static function url(): string
    {
        $url = getenv('HASHINBOX_URL');
        if (!is_string($url) || $url === '') {
            self::markTestSkipped('HASHINBOX_URL is not set');
        }
        return $url;
    }

    public function testHealth(): void
    {
        healthGet(new Client(self::url()));
        $this->addToAssertionCount(1);
    }

    public function testAKeySession(): void
    {
        $url = self::url();
        [$id, $value] = [getenv('HASHINBOX_KEY_ID'), getenv('HASHINBOX_KEY_SECRET')];
        if (!is_string($id) || !is_string($value)) {
            self::markTestSkipped('HASHINBOX_KEY_ID and HASHINBOX_KEY_SECRET are not set');
        }
        $api = new Client($url, secret: Secret::parse($id, $value));
        $read = sessionGet($api);
        self::assertSame($id, $read->key?->id);
        self::assertSame($id, $api->session()?->issuer);
    }
}
