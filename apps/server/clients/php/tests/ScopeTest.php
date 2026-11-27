<?php

declare(strict_types=1);

namespace Hashinbox\Tests;

use Hashinbox\Json;
use Hashinbox\Scope;
use Hashinbox\ScopeError;
use Hashinbox\Scopes;
use PHPUnit\Framework\Attributes\DataProvider;
use PHPUnit\Framework\TestCase;

final class ScopeTest extends TestCase
{
    /** @return list<array{array<string, mixed>}> */
    public static function parse(): array
    {
        return Golden::cases('scope.json', 'parse');
    }

    /** @return list<array{string}> */
    public static function refuse(): array
    {
        return Json::list(Golden::load('scope.json')['refuse'], static fn(mixed $text): array => [Json::string($text)]);
    }

    /** @return list<array{array<string, mixed>}> */
    public static function covers(): array
    {
        return Golden::cases('scope.json', 'covers');
    }

    /** @return list<array{array<string, mixed>}> */
    public static function escape(): array
    {
        return Golden::cases('scope.json', 'escape');
    }

    /** @return list<array{array<string, mixed>}> */
    public static function firstUncovered(): array
    {
        return Golden::cases('scope.json', 'first_uncovered');
    }

    /** @return list<array{array<string, mixed>}> */
    public static function sets(): array
    {
        return Golden::cases('scope.json', 'sets');
    }

    /** @return list<array{array<string, mixed>}> */
    public static function unpack(): array
    {
        return Golden::cases('scope.json', 'unpack');
    }

    /** @return list<array{array<string, mixed>}> */
    public static function expand(): array
    {
        return Json::list(Json::object(Golden::load('scope.json')['expand'])['cases'], static fn(mixed $case): array => [Json::object($case)]);
    }

    /** @param array<string, mixed> $case */
    #[DataProvider('parse')]
    public function testParse(array $case): void
    {
        self::assertSame(Json::string($case['display']), (string) Scope::parse(Json::string($case['text'])));
    }

    #[DataProvider('refuse')]
    public function testRefuse(string $text): void
    {
        $this->expectException(ScopeError::class);
        Scope::parse($text);
    }

    /** @param array<string, mixed> $case */
    #[DataProvider('covers')]
    public function testCovers(array $case): void
    {
        $ours = Scope::parse(Json::string($case['ours']));
        self::assertSame(Json::bool($case['covers']), $ours->covers(Scope::parse(Json::string($case['theirs']))));
    }

    /** @param array<string, mixed> $case */
    #[DataProvider('escape')]
    public function testEscape(array $case): void
    {
        [$id, $text] = [Json::string($case['id']), Json::string($case['text'])];
        self::assertSame($text, Scope::escapeId($id));
        self::assertSame("a[{$text}]:read", Scope::fill('a[{id}]:read', ['id' => $id]));
    }

    /** @param array<string, mixed> $case */
    #[DataProvider('firstUncovered')]
    public function testFirstUncovered(array $case): void
    {
        $granted = Scopes::parse(Json::string($case['granted']));
        $first = $granted->firstUncovered(Scopes::parse(Json::string($case['asked'])));
        self::assertSame($case['first_uncovered'], $first === null ? null : (string) $first);
    }

    /** @param array<string, mixed> $case */
    #[DataProvider('sets')]
    public function testSets(array $case): void
    {
        $scopes = Scopes::parse(Json::string($case['scopes']));
        self::assertSame(Json::string($case['display']), (string) $scopes);
        self::assertSame(Json::string($case['compressed']), (string) $scopes->compress());
        self::assertSame(Json::object($case['packed']), $scopes->pack());
        self::assertSame((string) $scopes->compress(), (string) Scopes::parse(Json::object($case['packed'])));
    }

    /** @param array<string, mixed> $case */
    #[DataProvider('unpack')]
    public function testUnpack(array $case): void
    {
        $packed = Json::object($case['packed']);
        if ($case['scopes'] === null) {
            $this->expectException(ScopeError::class);
        }
        self::assertSame($case['scopes'], (string) Scopes::parse($packed));
    }

    /** @param array<string, mixed> $case */
    #[DataProvider('expand')]
    public function testExpand(array $case): void
    {
        $depends = Json::object(Json::object(Golden::load('scope.json')['expand'])['depends']);
        $depends = array_map(static fn(mixed $targets): array => Json::list($targets, Json::string(...)), $depends);
        $expanded = Scopes::parse(Json::string($case['scopes']))->expand($depends);
        self::assertSame(Json::string($case['expanded']), (string) $expanded);
    }

    public function testAHoleNeedsItsParameter(): void
    {
        $this->expectException(\InvalidArgumentException::class);
        Scope::fill('a[{id}]:read', []);
    }
}
