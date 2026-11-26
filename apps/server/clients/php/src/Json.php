<?php

declare(strict_types=1);

namespace Hashinbox;

use JsonException;
use TypeError;
use UnexpectedValueException;
use ValueError;

/**
 * Hand-written: what the generated models call to read JSON (as `json_decode`
 * gives it: objects as arrays) and write it. Each reader checks the value's
 * type and throws an `UnexpectedValueException` when it is not.
 */
final class Json
{
    /**
     * `$text` decoded, objects as arrays.
     *
     * @throws JsonException
     */
    public static function decode(string $text): mixed
    {
        return json_decode($text, true, 512, JSON_THROW_ON_ERROR);
    }

    /**
     * `$value` as JSON text: slashes and Unicode unescaped, as serde_json writes them.
     *
     * @throws JsonException
     */
    public static function dump(mixed $value): string
    {
        $flags = JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE | JSON_UNESCAPED_LINE_TERMINATORS;
        return json_encode($value, $flags | JSON_PRESERVE_ZERO_FRACTION | JSON_THROW_ON_ERROR);
    }

    /**
     * The field `$name` of `$data`, which must be there (it may be null).
     *
     * @param array<string, mixed> $data
     */
    public static function field(array $data, string $name): mixed
    {
        if (!array_key_exists($name, $data)) {
            throw new UnexpectedValueException("missing field `{$name}`");
        }
        return $data[$name];
    }

    public static function string(mixed $value): string
    {
        return is_string($value) ? $value : throw self::mismatch('a string', $value);
    }

    public static function int(mixed $value): int
    {
        return is_int($value) ? $value : throw self::mismatch('an integer', $value);
    }

    public static function float(mixed $value): float
    {
        return is_int($value) || is_float($value) ? (float) $value : throw self::mismatch('a number', $value);
    }

    public static function bool(mixed $value): bool
    {
        return is_bool($value) ? $value : throw self::mismatch('a boolean', $value);
    }

    /**
     * A JSON object, as an array.
     *
     * @return array<string, mixed>
     */
    public static function object(mixed $value): array
    {
        if (!is_array($value)) {
            throw self::mismatch('an object', $value);
        }
        /** @var array<string, mixed> $value */
        return $value;
    }

    /**
     * Each item of the JSON array `$value`, decoded.
     *
     * @template T
     * @param callable(mixed): T $decode
     * @return list<T>
     */
    public static function list(mixed $value, callable $decode): array
    {
        if (!is_array($value) || !array_is_list($value)) {
            throw self::mismatch('an array', $value);
        }
        return array_map($decode, $value);
    }

    /**
     * Each value of the JSON object `$value`, decoded. PHP turns a key like
     * `"7"` into the integer 7.
     *
     * @template T
     * @param callable(mixed): T $decode
     * @return array<string, T>
     */
    public static function map(mixed $value, callable $decode): array
    {
        if (!is_array($value)) {
            throw self::mismatch('an object', $value);
        }
        $out = [];
        foreach ($value as $key => $item) {
            $out[(string) $key] = $decode($item);
        }
        return $out;
    }

    /**
     * `$value` decoded by the first of `$decoders` that takes it: a union.
     *
     * @template T
     * @param callable(mixed): T ...$decoders
     * @return T
     */
    public static function first(mixed $value, callable ...$decoders): mixed
    {
        foreach ($decoders as $decode) {
            try {
                return $decode($value);
            } catch (UnexpectedValueException | ValueError | TypeError) {
                continue;
            }
        }
        throw new UnexpectedValueException('no variant fits');
    }

    /**
     * A union's `$value` as JSON: an enum its value, a model its fields (as an
     * object, so an empty one stays `{}`).
     */
    public static function encode(mixed $value): mixed
    {
        if ($value instanceof \BackedEnum) {
            return $value->value;
        }
        if (is_object($value) && method_exists($value, 'toJson')) {
            return (object) $value->toJson();
        }
        return $value;
    }

    private static function mismatch(string $expected, mixed $value): UnexpectedValueException
    {
        return new UnexpectedValueException("expected {$expected}, got " . get_debug_type($value));
    }
}
