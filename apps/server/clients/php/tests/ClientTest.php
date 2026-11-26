<?php

declare(strict_types=1);

namespace Hashinbox\Tests;

use Hashinbox\ApiError;
use Hashinbox\Client;
use Hashinbox\CreateWalletRequest;
use Hashinbox\Json;
use Hashinbox\Kind;
use Hashinbox\LogsStreamRequest;
use Hashinbox\MissingScope;
use Hashinbox\NetworkTipSearchRequest;
use Hashinbox\Scopes;
use Hashinbox\Secret;
use Hashinbox\Session;
use Hashinbox\WalletGetRequest;
use Hashinbox\WalletsCreateRequest;
use Hashinbox\WalletsListRequest;
use PHPUnit\Framework\TestCase;
use RuntimeException;

use function Hashinbox\logsStream;
use function Hashinbox\networkTipSearch;
use function Hashinbox\walletGet;
use function Hashinbox\walletsCreate;

/**
 * @phpstan-import-type Sent from FakeTransport
 */
final class ClientTest extends TestCase
{
    private const WALLET = '{"id": "ethereum.mainnet.0xabc", "chain": "ethereum", "network": "mainnet", "address": "0xabc",'
        . ' "external_id": null, "indexes": {}, "keyed": true, "tags": []}';

    private static function secret(): Secret
    {
        $vectors = Golden::load('session.json');
        return Secret::parse(Json::string($vectors['id']), Json::string($vectors['secret']));
    }

    /**
     * @param Sent $sent
     */
    private static function bearer(array $sent): Session
    {
        return Session::parse(substr($sent['headers']['Authorization'] ?? '', strlen('Bearer ')));
    }

    /**
     * @param array<string, mixed> $error
     * @return array{int, string}
     */
    private static function error(int $status, array $error): array
    {
        return [$status, Json::dump($error)];
    }

    public function testPathQueryBodyAndBearer(): void
    {
        $fake = new FakeTransport(static fn(array $sent): array => $sent['method'] === 'POST'
            ? [201, self::WALLET]
            : [200, '{"items": [], "next_cursor": null}']);
        $session = self::secret()->issue(Scopes::parse('com.hashinbox.wallets.$*'));
        $api = new Client('http://hashinbox.test/', session: $session, transport: $fake);
        walletsCreate($api, new WalletsCreateRequest(new CreateWalletRequest(chain: 'ethereum', network: 'mainnet')));
        $api->send(new WalletsListRequest(limit: 5));
        [$post, $get] = $fake->sent;
        self::assertSame('http://hashinbox.test/v1/wallets', $post['url']);
        self::assertSame('{"chain":"ethereum","network":"mainnet"}', $post['body']);
        self::assertSame('application/json', $post['headers']['Content-Type']);
        self::assertSame("Bearer {$session->token}", $post['headers']['Authorization']);
        self::assertSame('http://hashinbox.test/v1/wallets?limit=5', $get['url']);
        self::assertNull($get['body']);
    }

    public function testAPathParameterIsEscaped(): void
    {
        $fake = new FakeTransport(static fn(array $sent): array => [200, self::WALLET]);
        $session = self::secret()->issue(Scopes::parse('com.hashinbox.wallets.$read'));
        $wallet = walletGet(new Client('http://hashinbox.test', $session, transport: $fake), new WalletGetRequest('a/b c'));
        self::assertSame('0xabc', $wallet->address);
        self::assertSame([], $wallet->indexes);
        self::assertSame('http://hashinbox.test/v1/wallets/a%2Fb%20c', $fake->sent[0]['url']);
    }

    public function testAnErrorIsMapped(): void
    {
        $fake = new FakeTransport(static fn(array $sent): array => self::error(403, [
            'kind' => 'permission_denied',
            'message' => 'missing scope',
            'reason' => 'missing_scope',
            'metadata' => ['scope' => 'com.hashinbox.wallets.$read'],
        ]));
        try {
            (new Client('http://hashinbox.test', checkScopes: false, transport: $fake))->send(new WalletGetRequest('x'));
            self::fail('no error');
        } catch (ApiError $error) {
            self::assertSame([403, Kind::PermissionDenied, 'missing_scope'], [$error->status, $error->kind, $error->reason]);
            self::assertSame('missing scope', $error->getMessage());
            self::assertSame(['scope' => 'com.hashinbox.wallets.$read'], $error->metadata);
        }
    }

    public function testAMissingScopeIsRefusedLocally(): void
    {
        $fake = new FakeTransport(static fn(array $sent): array => throw new \LogicException('sent'));
        $session = self::secret()->issue(Scopes::parse('com.hashinbox.wallets[x].$read'));
        try {
            (new Client('http://hashinbox.test', $session, transport: $fake))->send(new WalletGetRequest('y'));
            self::fail('sent');
        } catch (MissingScope $missing) {
            self::assertSame('com.hashinbox.wallets[y].$read', $missing->scope);
        }
    }

    public function testASecretSignsItsOwnSessionsAndReSignsOnExpiry(): void
    {
        $secret = self::secret();
        $vectors = Golden::load('session.json');
        $read = [
            'kind' => 'key',
            'user' => ['id' => 'u1', 'email' => 'a@b.c', 'name' => 'a', 'login' => true, 'scopes' => (object) [], 'created_at' => 0],
            'key' => [
                'id' => $secret->id,
                'user_id' => 'u1',
                'name' => 'shop',
                'scopes' => ['com.hashinbox' => ['wallets.$read']],
                'created_at' => 0,
            ],
            'sub' => null,
            'scopes' => (object) [],
            'issued_at' => 0,
            'expires_at' => 0,
            'amr' => [],
        ];
        $sent = [];
        $fake = new FakeTransport(static function (array $request) use (&$sent, $read, $vectors): array {
            self::assertStringNotContainsString(Json::string($vectors['secret']), $request['headers']['Authorization'] ?? '');
            if (str_ends_with($request['url'], '/v1/session')) {
                self::assertSame('', (string) self::bearer($request)->scopes);
                return [200, Json::dump($read)];
            }
            $sent[] = self::bearer($request);
            return count($sent) === 1
                ? self::error(401, ['kind' => 'unauthenticated', 'message' => 'expired', 'reason' => 'session_expired'])
                : [200, self::WALLET];
        });
        $api = new Client('http://hashinbox.test', secret: $secret, transport: $fake);
        $api->send(new WalletGetRequest('x'));
        self::assertCount(2, $sent);
        [$first, $second] = $sent;
        self::assertSame('com.hashinbox.wallets.$read', (string) $first->scopes);
        self::assertSame(600, $first->expiresAt - $first->issuedAt);
        self::assertSame($second->token, $api->session()?->token);
        // Within a minute of its expiry, a new one is signed.
        $api->setSession($secret->issue(Scopes::parse('com.hashinbox.wallets.$read'), ttl: 30));
        $api->send(new WalletGetRequest('x'));
        self::assertGreaterThan(time() + 500, $sent[2]->expiresAt);
        self::assertCount(4, $fake->sent);
    }

    public function testLoginKeepsTheSessionAndSurfacesTotpRequired(): void
    {
        $user = Json::object(Golden::load('session.json')['user']);
        $fake = new FakeTransport(static function (array $request) use ($user): array {
            self::assertArrayNotHasKey('Authorization', $request['headers']);
            $body = Json::object(Json::decode((string) $request['body']));
            if (!isset($body['totp'])) {
                return self::error(401, ['kind' => 'unauthenticated', 'message' => 'code needed', 'reason' => 'totp_required']);
            }
            return [201, Json::dump(['token' => $user['token'], 'expires_at' => $user['exp'], 'scopes' => (object) []])];
        });
        $api = new Client('http://hashinbox.test', transport: $fake);
        try {
            $api->login('a@b.c', password: 'pw');
            self::fail('logged in');
        } catch (ApiError $error) {
            self::assertSame('totp_required', $error->reason);
        }
        $issued = $api->login('a@b.c', password: 'pw', totp: '123456');
        self::assertSame($user['token'], $issued->token);
        self::assertSame('user/0193a', $api->session()?->issuer);
        $api->logout();
        self::assertNull($api->session());
        self::assertSame('{"email":"a@b.c","password":"pw","totp":"123456"}', $fake->sent[1]['body']);
    }

    public function testAStreamYieldsItsEvents(): void
    {
        $chunks = [": keep-alive\n\n", "data: {\"id\": 0}\n", "\nevent: missed\r\n", "data: {\"missed\": 2}\r\n\r\n"];
        $fake = new FakeTransport(static function (array $request) use ($chunks): array {
            self::assertSame('text/event-stream', $request['headers']['Accept']);
            self::assertTrue($request['stream']);
            return [200, $chunks];
        });
        $api = new Client('http://hashinbox.test', self::secret()->issue(Scopes::parse('com.hashinbox.*.$*')), transport: $fake);
        $raw = iterator_to_array(logsStream($api), false);
        self::assertSame(['message', 'missed'], array_map(static fn($event): string => $event->event, $raw));
        $typed = iterator_to_array($api->stream(new LogsStreamRequest()), false);
        self::assertSame([['id' => 0], ['missed' => 2]], array_map(static fn($event): mixed => $event->data, $typed));
    }

    /**
     * A client over `$answers`, one per request, in order (a closure answers
     * or throws).
     *
     * @param list<array{int, string|list<string>}|RuntimeException> $answers
     * @return array{Client, FakeTransport}
     */
    private static function streaming(array $answers): array
    {
        $fake = new FakeTransport(static function (array $request) use (&$answers): array {
            $answer = array_shift($answers) ?? [500, ''];
            if ($answer instanceof RuntimeException) {
                throw $answer;
            }
            return $answer;
        });
        $scopes = Scopes::parse('com.hashinbox.*.$*');
        $api = new Client('http://hashinbox.test', self::secret()->issue($scopes), transport: $fake, backoff: 0.01, backoffMax: 0.02);
        return [$api, $fake];
    }

    /**
     * The `cursor` each request sent, null: none.
     *
     * @return list<?string>
     */
    private static function cursors(FakeTransport $fake): array
    {
        return array_map(static function (array $request): ?string {
            parse_str((string) parse_url($request['url'], PHP_URL_QUERY), $query);
            $cursor = $query['cursor'] ?? null;
            return is_string($cursor) ? $cursor : null;
        }, $fake->sent);
    }

    public function testAStreamGoesOnAcrossWindowsFromItsCursor(): void
    {
        [$api, $fake] = self::streaming([
            [200, ["id: 1\ndata: {\"n\":1}\n\n", "id: 2\ndata: {\"n\":2}\n\n", "event: close\ndata: {}\nretry: 0\n\n"]],
            [200, ["id: 3\ndata: {\"n\":3}\n\nevent: close\ndata: {}\n\n"]],
            [200, "id: 4\nevent: found\ndata: {\"n\":4}\n\n"],
        ]);
        $stream = $api->stream(new LogsStreamRequest(level: 'warn'));
        $read = array_map(static fn($event): mixed => $event->data, iterator_to_array($stream, false));
        self::assertSame([['n' => 1], ['n' => 2], ['n' => 3], ['n' => 4]], $read);
        self::assertSame('4', $stream->cursor());
        self::assertSame([null, '2', '3'], self::cursors($fake), 'each window from the last id; ended without close');
        self::assertStringContainsString('level=warn', $fake->sent[1]['url'], 'the filters kept');
        self::assertArrayNotHasKey('Last-Event-ID', $fake->sent[1]['headers']);
    }

    public function testStopEndsAStreamWithoutReconnecting(): void
    {
        $window = [200, "id: 1\ndata: {}\n\nid: 2\ndata: {}\n\nevent: close\ndata: {}\n\n"];
        [$api, $fake] = self::streaming([$window, $window, $window]);
        $handle = logsStream($api);
        $read = [];
        foreach ($handle as $event) {
            $read[] = $event->id;
            if (count($read) === 3) {
                $handle->stop();
            }
        }
        self::assertSame(['1', '2', '1'], $read);
        self::assertCount(2, $fake->sent);
        self::assertSame([], iterator_to_array($handle, false), 'stopped for good');
        self::assertCount(2, $fake->sent);
    }

    public function testAStreamBacksOffOnErrorsAndThrowsARefusal(): void
    {
        $error = '{"kind": "unavailable", "message": "down"}';
        [$api, $fake] = self::streaming([
            [503, $error],
            new RuntimeException('curl: reset'),
            [200, "id: 7\nevent: probe\ndata: {}\n\nevent: close\ndata: {}\n\n"],
            [401, '{"kind": "unauthenticated", "message": "refused", "reason": "session_revoked"}'],
        ]);
        $started = microtime(true);
        $handle = networkTipSearch($api, new NetworkTipSearchRequest(chain: 'bitcoin', network: 'regtest', at: '1', cursor: '5'));
        $read = [];
        try {
            foreach ($handle as $event) {
                $read[] = $event->event;
            }
            self::fail('a 401 is thrown');
        } catch (ApiError $refused) {
            self::assertSame(401, $refused->status);
        }
        self::assertSame(['probe'], $read);
        self::assertGreaterThanOrEqual(0.03, microtime(true) - $started, 'waited 0.01 s, then 0.02 s');
        self::assertCount(4, $fake->sent);
        self::assertSame(['5', '5', '5', '7'], self::cursors($fake), "the caller's cursor first, then the last id");
        self::assertSame(1, substr_count($fake->sent[3]['url'], 'cursor='), 'overridden, not repeated');
    }
}
