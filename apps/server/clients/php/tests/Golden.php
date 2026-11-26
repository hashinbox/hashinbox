<?php

declare(strict_types=1);

namespace Hashinbox\Tests;

use Hashinbox\Json;

/**
 * The shared vectors, `apps/server/clients/_golden/`.
 */
final class Golden
{
    /**
     * @return array<string, mixed>
     */
    public static function load(string $name): array
    {
        return Json::object(Json::decode((string) file_get_contents(__DIR__ . "/../../_golden/{$name}")));
    }

    /**
     * Each case of `$key` in `$name`, as a data provider's rows.
     *
     * @return list<array{array<string, mixed>}>
     */
    public static function cases(string $name, string $key): array
    {
        return Json::list(self::load($name)[$key], static fn(mixed $case): array => [Json::object($case)]);
    }
}
