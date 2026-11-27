<?php

declare(strict_types=1);

namespace Hashinbox;

use Stringable;

/**
 * Hand-written: one step of a scope's path: a resource, all of it
 * (`wallets`), or only the one named (`wallets[a]`).
 */
final readonly class Segment implements Stringable
{
    public function __construct(public string $name, public ?string $id = null) {}

    /**
     * @throws ScopeError
     */
    public static function parse(string $text): self
    {
        $open = strpos($text, '[');
        if ($open === false) {
            return self::isName($text) ? new self($text) : throw new ScopeError("`{$text}` is not a name");
        }
        $name = substr($text, 0, $open);
        $rest = substr($text, $open + 1);
        if (!str_ends_with($rest, ']')) {
            throw new ScopeError("bad id in `{$name}`");
        }
        $id = self::unescape(substr($rest, 0, -1));
        if ($id === '') {
            throw new ScopeError("bad id in `{$name}`");
        }
        return self::isName($name) ? new self($name, $id) : throw new ScopeError("`{$name}` is not a name");
    }

    /**
     * Whether `$text` is a name: ASCII letters, digits, `_` and `-`.
     */
    public static function isName(string $text): bool
    {
        return preg_match('/^[A-Za-z0-9_-]+$/D', $text) === 1;
    }

    /**
     * Whether it covers `$other`: the same name, and no id or the same.
     */
    public function covers(self $other): bool
    {
        return $this->name === $other->name && ($this->id === null || $this->id === $other->id);
    }

    /**
     * Rust's order: by name, then no id before an id, each by bytes.
     */
    public static function compare(self $a, self $b): int
    {
        $name = strcmp($a->name, $b->name) <=> 0;
        if ($name !== 0 || $a->id === $b->id) {
            return $name;
        }
        if ($a->id === null || $b->id === null) {
            return $a->id === null ? -1 : 1;
        }
        return strcmp($a->id, $b->id) <=> 0;
    }

    public function __toString(): string
    {
        return $this->id === null ? $this->name : "{$this->name}[" . Scope::escapeId($this->id) . ']';
    }

    /**
     * The id `$text` (as written between `[` and `]`) is: each `%XX` (or Rust's
     * `%+X`) a byte, the result UTF-8.
     *
     * @throws ScopeError
     */
    private static function unescape(string $text): string
    {
        $out = '';
        $length = strlen($text);
        for ($at = 0; $at < $length; $at++) {
            if ($text[$at] !== '%') {
                $out .= $text[$at];
                continue;
            }
            $hex = substr($text, $at + 1, 2);
            if (preg_match('/^(?:[0-9A-Fa-f]{2}|\+[0-9A-Fa-f])$/D', $hex) !== 1) {
                throw new ScopeError("bad escape in `{$text}`");
            }
            $out .= chr(intval($hex, 16));
            $at += 2;
        }
        return preg_match('//u', $out) === 1 ? $out : throw new ScopeError("bad escape in `{$text}`");
    }
}
