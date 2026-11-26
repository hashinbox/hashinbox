<?php

declare(strict_types=1);

namespace Hashinbox;

use ArrayIterator;
use Countable;
use IteratorAggregate;
use Stringable;
use Traversable;

/**
 * Hand-written: a set of scopes a key or a session holds; its text is theirs,
 * space-separated.
 *
 * @implements IteratorAggregate<int, Scope>
 */
final readonly class Scopes implements Countable, IteratorAggregate, Stringable
{
    /** Rust's `char::is_whitespace`, what `split_whitespace` splits on. */
    private const WHITESPACE = '/[\x{9}-\x{d}\x{20}\x{85}\x{a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}]+/u';

    /**
     * @param list<Scope> $items
     */
    public function __construct(public array $items = []) {}

    /**
     * Space-separated text (empty: none), or a packed claim (each shared
     * prefix and the scopes after it).
     *
     * @param string|array<array-key, mixed> $value
     * @throws ScopeError
     */
    public static function parse(string|array $value): self
    {
        if (is_string($value)) {
            $texts = preg_split(self::WHITESPACE, $value, -1, PREG_SPLIT_NO_EMPTY);
            if ($texts === false) {
                throw new ScopeError('scopes are not UTF-8 text');
            }
            return new self(array_map(Scope::parse(...), $texts));
        }
        $prefixes = array_map(strval(...), array_keys($value));
        sort($prefixes, SORT_STRING);
        $items = [];
        foreach ($prefixes as $prefix) {
            $texts = $value[$prefix];
            if (!is_array($texts)) {
                throw new ScopeError("packed scopes of `{$prefix}` are not a list");
            }
            foreach ($texts as $text) {
                if (!is_string($text)) {
                    throw new ScopeError("packed scopes of `{$prefix}` are not text");
                }
                $items[] = Scope::parse($prefix === '' ? $text : "{$prefix}.{$text}");
            }
        }
        return new self($items);
    }

    /**
     * Whether one of them covers `$scope`; text that does not parse is never
     * covered.
     */
    public function covers(Scope|string $scope): bool
    {
        if (is_string($scope)) {
            try {
                $scope = Scope::parse($scope);
            } catch (ScopeError) {
                return false;
            }
        }
        foreach ($this->items as $held) {
            if ($held->covers($scope)) {
                return true;
            }
        }
        return false;
    }

    /**
     * The first of `$others` none of them covers; null when they cover every one.
     */
    public function firstUncovered(self $others): ?Scope
    {
        foreach ($others->items as $scope) {
            if (!$this->covers($scope)) {
                return $scope;
            }
        }
        return null;
    }

    /**
     * The fewest scopes allowing the same, sorted.
     */
    public function compress(): self
    {
        $items = $this->items;
        $kept = [];
        foreach ($items as $at => $scope) {
            foreach ($items as $other => $held) {
                if ($other !== $at && $held->covers($scope) && (!$scope->covers($held) || $other < $at)) {
                    continue 2;
                }
            }
            $kept[] = $scope;
        }
        usort($kept, Scope::compare(...));
        return new self($kept);
    }

    /**
     * Compressed, then grouped by their shared prefix (the leading plain names
     * every scope starting with the same step shares, each keeping one step at
     * least): the JWT claim, sorted by prefix. PHP turns a prefix like `"123"`
     * into the integer key 123.
     *
     * @return array<string, list<string>>
     */
    public function pack(): array
    {
        $groups = [];
        foreach ($this->compress()->items as $scope) {
            $groups[(string) $scope->path[0]][] = $scope;
        }
        $packed = [];
        foreach ($groups as $scopes) {
            $shared = self::sharedPrefix($scopes);
            foreach ($scopes as $scope) {
                $steps = array_map(static fn(Segment $step): string => (string) $step, array_slice($scope->path, 0, $shared));
                $rest = new Scope(array_slice($scope->path, $shared), $scope->action);
                $packed[implode('.', $steps)][] = (string) $rest;
            }
        }
        ksort($packed, SORT_STRING);
        return $packed;
    }

    public function getIterator(): Traversable
    {
        return new ArrayIterator($this->items);
    }

    public function count(): int
    {
        return count($this->items);
    }

    public function __toString(): string
    {
        return implode(' ', array_map(static fn(Scope $scope): string => (string) $scope, $this->items));
    }

    /**
     * How many leading steps of `$scopes` are the same plain name in each, each
     * scope keeping one step at least.
     *
     * @param non-empty-list<Scope> $scopes
     */
    private static function sharedPrefix(array $scopes): int
    {
        $shortest = min(array_map(static fn(Scope $scope): int => count($scope->path), $scopes));
        $first = $scopes[0]->path;
        for ($at = 0; $at < $shortest - 1; $at++) {
            $step = $first[$at];
            if ($step->name === Scope::ANY || $step->id !== null) {
                return $at;
            }
            foreach ($scopes as $scope) {
                if ($scope->path[$at]->name !== $step->name || $scope->path[$at]->id !== $step->id) {
                    return $at;
                }
            }
        }
        return max($shortest - 1, 0);
    }
}
