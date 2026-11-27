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
     * prefix and the scopes after it, one as text or several as a list; `*`
     * the prefix itself).
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
            $texts = is_string($texts) ? [$texts] : $texts;
            if (!is_array($texts)) {
                throw new ScopeError("packed scopes of `{$prefix}` are not text or a list");
            }
            foreach ($texts as $text) {
                if (!is_string($text)) {
                    throw new ScopeError("packed scopes of `{$prefix}` are not text");
                }
                $items[] = Scope::parse(self::joined($prefix, $text));
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
     * Them and every scope `$depends` adds, to a fixed point, compressed.
     * `$depends` maps a source path (`com.hashinbox.triggers`) to the scopes
     * holding it needs (`com.hashinbox.wallets`): the generated `DEPENDS`. A
     * held scope triggers a source when it covers the source or an id of it
     * (`triggers`, `triggers[t1]`, `com.hashinbox`; not `triggers.runs`), and
     * adds each target whole: read only when the target is written `:read` or
     * the held scope is read only. An entry whose text does not parse adds
     * nothing.
     *
     * @param array<string, list<string>> $depends
     */
    public function expand(array $depends): self
    {
        $parsed = [];
        foreach ($depends as $source => $targets) {
            try {
                $parsed[] = [Scope::parse((string) $source), array_map(Scope::parse(...), $targets)];
            } catch (ScopeError) {
                continue;
            }
        }
        $held = $this->items;
        for ($at = 0; $at < count($held); $at++) {
            $scope = $held[$at];
            $plain = new Scope(array_map(static fn(Segment $step): Segment => new Segment($step->name), $scope->path));
            foreach ($parsed as [$source, $targets]) {
                if (!$plain->reaches($source->path)) {
                    continue;
                }
                foreach ($targets as $target) {
                    $target = new Scope($target->path, $target->read || $scope->read);
                    if (!(new self($held))->covers($target)) {
                        $held[] = $target;
                    }
                }
            }
        }
        return (new self($held))->compress();
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
     * every scope starting with the same step shares; a scope that is its
     * prefix is `*`, `*:read`): the JWT claim, sorted by prefix. A group of one
     * scope is its text alone: `{"com.hashinbox": "*"}`. PHP turns a prefix
     * like `"123"` into the integer key 123.
     *
     * @return array<string, string|list<string>>
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
                $steps = array_map(strval(...), array_slice($scope->path, 0, $shared));
                $rest = (string) new Scope(array_slice($scope->path, $shared), $scope->read);
                $packed[implode('.', $steps)][] = count($scope->path) === $shared ? "*{$rest}" : $rest;
            }
        }
        ksort($packed, SORT_STRING);
        return array_map(static fn(array $texts): string|array => count($texts) === 1 ? $texts[0] : $texts, $packed);
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

    /** `$text` under `$prefix`: `*` (`*:read`) the prefix itself. */
    private static function joined(string $prefix, string $text): string
    {
        if ($prefix === '') {
            return $text;
        }
        $level = substr($text, 1);
        if (str_starts_with($text, '*') && ($level === '' || str_starts_with($level, ':'))) {
            return $prefix . $level;
        }
        return "{$prefix}.{$text}";
    }

    /**
     * How many leading steps of `$scopes` are the same plain name in each.
     *
     * @param non-empty-list<Scope> $scopes
     */
    private static function sharedPrefix(array $scopes): int
    {
        $shortest = min(array_map(static fn(Scope $scope): int => count($scope->path), $scopes));
        $first = $scopes[0]->path;
        for ($at = 0; $at < $shortest; $at++) {
            $step = $first[$at];
            if ($step->id !== null) {
                return $at;
            }
            foreach ($scopes as $scope) {
                if ($scope->path[$at]->name !== $step->name || $scope->path[$at]->id !== $step->id) {
                    return $at;
                }
            }
        }
        return $shortest;
    }
}
