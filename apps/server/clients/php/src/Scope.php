<?php

declare(strict_types=1);

namespace Hashinbox;

use Closure;
use InvalidArgumentException;
use Stringable;

/**
 * Hand-written: `packages/scope` ported. A scope is text
 * (`com.hashinbox.wallets[w1]:read`): a path of resources, full (read and
 * write) or read only. It covers its whole subtree, a set packs into the JWT
 * `scopes` claim, and a template's `{name}` holes are filled with escaped ids.
 * The vectors in `apps/server/clients/_golden/scope.json` keep this port
 * honest.
 */
final readonly class Scope implements Stringable
{
    /** The suffix of a read-only scope. */
    private const READ = ':read';

    /**
     * @param list<Segment> $path Its steps, one at least.
     * @param bool $read Whether it is read only (`:read`); otherwise full.
     */
    public function __construct(public array $path, public bool $read = false) {}

    /**
     * `$text`: steps split by `.` (not inside `[` `]`), then `:read` or
     * nothing.
     *
     * @throws ScopeError
     */
    public static function parse(string $text): self
    {
        $steps = [];
        $start = 0;
        $inside = false;
        $end = strlen($text);
        for ($at = 0; $at < $end; $at++) {
            $char = $text[$at];
            if ($char === '[' && !$inside) {
                $inside = true;
            } elseif ($char === ']' && $inside) {
                $inside = false;
            } elseif ($char === '.' && !$inside) {
                $steps[] = substr($text, $start, $at - $start);
                $start = $at + 1;
            } elseif ($char === ':' && !$inside) {
                $end = $at;
                break;
            }
        }
        if ($inside) {
            throw new ScopeError("bad id in `{$text}`");
        }
        $level = substr($text, $end);
        if ($level !== '' && $level !== self::READ) {
            throw new ScopeError("`{$text}` does not end with `:read` or a step");
        }
        if ($text === '') {
            throw new ScopeError('a scope needs a resource');
        }
        $steps[] = substr($text, $start, $end - $start);
        return new self(array_map(Segment::parse(...), $steps), $level === self::READ);
    }

    /**
     * Whether it allows everything `$other` allows: it is full or `$other` is
     * read only, and its path is a prefix of `$other`'s, step by step. It so
     * covers its whole subtree; a child never covers its parent.
     */
    public function covers(self $other): bool
    {
        return (!$this->read || $other->read) && $this->reaches($other->path);
    }

    /**
     * Whether its path is a prefix of `$path`, step by step.
     *
     * @param list<Segment> $path
     */
    public function reaches(array $path): bool
    {
        if (count($this->path) > count($path)) {
            return false;
        }
        foreach ($this->path as $at => $step) {
            if (!$step->covers($path[$at])) {
                return false;
            }
        }
        return true;
    }

    /**
     * Rust's order: by path (step by step, a prefix first), then full before
     * read only.
     */
    public static function compare(self $a, self $b): int
    {
        foreach ($a->path as $at => $step) {
            if (!isset($b->path[$at])) {
                return 1;
            }
            $order = Segment::compare($step, $b->path[$at]);
            if ($order !== 0) {
                return $order;
            }
        }
        if (count($a->path) !== count($b->path)) {
            return -1;
        }
        return $a->read <=> $b->read;
    }

    /**
     * `$id` as written between a scope's brackets: `%`, `[`, `]`, whitespace
     * and controls as `%XX`.
     */
    public static function escapeId(string $id): string
    {
        return (string) preg_replace_callback(
            '/[%\[\]\x00-\x20\x7f]/',
            static fn($match): string => sprintf('%%%02X', ord($match[0])),
            $id,
        );
    }

    /**
     * `$template` with each `{name}` its parameter, escaped by `$escape`
     * (`escapeId` by default).
     *
     * @param array<string, string> $params
     * @param (Closure(string): string)|null $escape
     * @throws InvalidArgumentException a hole without its parameter
     */
    public static function fill(string $template, array $params, ?Closure $escape = null): string
    {
        $escape ??= self::escapeId(...);
        return (string) preg_replace_callback(
            '/\{([^}]+)\}/',
            static fn($match): string => isset($params[$match[1]])
                ? $escape($params[$match[1]])
                : throw new InvalidArgumentException("`{$template}` needs `{$match[1]}`"),
            $template,
        );
    }

    public function __toString(): string
    {
        return implode('.', array_map(strval(...), $this->path)) . ($this->read ? self::READ : '');
    }
}
