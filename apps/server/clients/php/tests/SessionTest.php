<?php

declare(strict_types=1);

namespace Hashinbox\Tests;

use Hashinbox\Amr;
use Hashinbox\Json;
use Hashinbox\Scopes;
use Hashinbox\Secret;
use Hashinbox\Session;
use InvalidArgumentException;
use PHPUnit\Framework\Attributes\DataProvider;
use PHPUnit\Framework\TestCase;

use const Hashinbox\DEPENDS;
use const Hashinbox\WALLET_GET;

final class SessionTest extends TestCase
{
    /** @return list<array{array<string, mixed>}> */
    public static function sessions(): array
    {
        return Golden::cases('session.json', 'sessions');
    }

    private static function secret(): Secret
    {
        $vectors = Golden::load('session.json');
        return Secret::parse(Json::string($vectors['id']), Json::string($vectors['secret']));
    }

    /** @param array<string, mixed> $case */
    #[DataProvider('sessions')]
    public function testSigningIsByteIdentical(array $case): void
    {
        $iat = Json::int($case['iat']);
        $subject = $case['sub'] === null ? null : Json::string($case['sub']);
        $scopes = Scopes::parse(Json::string($case['scopes']));
        $session = self::secret()->issue($scopes, $subject, Json::int($case['exp']) - $iat, $iat);
        self::assertSame(Json::string($case['token']), $session->token);
        self::assertSame(self::secret()->id, $session->issuer);
        self::assertSame($subject, $session->subject);
        self::assertSame((string) $scopes->expand(DEPENDS), (string) $session->scopes);
    }

    public function testAServerMadeTokenParses(): void
    {
        $user = Json::object(Golden::load('session.json')['user']);
        $session = Session::parse(Json::string($user['token']));
        self::assertSame([$user['iss'], $user['sub']], [$session->issuer, $session->subject]);
        self::assertSame([$user['iat'], $user['exp']], [$session->issuedAt, $session->expiresAt]);
        self::assertSame($user['scopes'], (string) $session->scopes);
        self::assertSame([Amr::Pwd, Amr::Otp], $session->amr);
        self::assertTrue($session->expired(Json::int($user['exp'])));
        self::assertFalse($session->expired(Json::int($user['exp']) - 1));
        self::assertTrue($session->can(WALLET_GET, ['id' => 'a] b']));
    }

    public function testASessionChecksItsScopes(): void
    {
        $session = self::secret()->issue(Scopes::parse('com.hashinbox.wallets[x]:read'));
        self::assertTrue($session->can(WALLET_GET, ['id' => 'x']));
        self::assertFalse($session->can(WALLET_GET, ['id' => 'y']));
        self::assertSame(600, $session->expiresAt - $session->issuedAt);
    }

    public function testAMistypedSecretIsRefused(): void
    {
        $vectors = Golden::load('session.json');
        [$id, $value] = [Json::string($vectors['id']), Json::string($vectors['secret'])];
        $typo = substr($value, 0, 10) . ($value[10] !== 'a' ? 'a' : 'b') . substr($value, 11);
        foreach ([[$id, $typo], [$id, 'his/0OIl'], ['key', $value]] as [$badId, $badValue]) {
            try {
                Secret::parse($badId, $badValue);
                self::fail("{$badId} {$badValue} parsed");
            } catch (InvalidArgumentException) {
                $this->addToAssertionCount(1);
            }
        }
        self::assertStringNotContainsString($value, print_r(self::secret(), true));
    }

    public function testATtlPastADayIsRefused(): void
    {
        $this->expectException(InvalidArgumentException::class);
        self::secret()->issue(ttl: Secret::MAX_TTL + 1);
    }
}
