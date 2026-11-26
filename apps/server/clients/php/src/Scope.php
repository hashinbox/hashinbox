<?php

declare(strict_types=1);

namespace Hashinbox;

use Closure;
use InvalidArgumentException;
use Stringable;

/**
 * Hand-written: `packages/scope` ported. A scope is text
 * (`com.hashinbox.wallets[w1].$read`): a path of resources and an action. One
 * covers another, a set packs into the JWT `scopes` claim, and a template's
 * `{name}` holes are filled with escaped ids. The vectors in
 * `apps/server/clients/_golden/scope.json` keep this port honest.
 */
final readonly class Scope implements Stringable
{
    /** `*`: any resource (a segment's name) or any action. */
    public const ANY = '*';

    /**
     * @param list<Segment> $path Its steps, one at least.
     * @param string $action Its action; `Scope::ANY` for `$*`.
     */
    public function __construct(public array $path, public string $action) {}

    /**
     * `$text`: steps split by `.` (not inside `[` `]`), the last one the
     * action, `$name` or `$*`.
     *
     * @throws ScopeError
     */
    public static function parse(string $text): self
    {
        $steps = [];
        $start = 0;
        $inside = false;
        $length = strlen($text);
        for ($at = 0; $at < $length; $at++) {
            $char = $text[$at];
            if ($char === '[' && !$inside) {
                $inside = true;
            } elseif ($char === ']' && $inside) {
                $inside = false;
            } elseif ($char === '.' && !$inside) {
                $steps[] = substr($text, $start, $at - $start);
                $start = $at + 1;
            }
        }
        if ($inside) {
            throw new ScopeError("bad id in `{$text}`");
        }
        $last = substr($text, $start);
        if (!str_starts_with($last, '$')) {
            throw new ScopeError("`{$text}` does not end with `\$action`");
        }
        $action = substr($last, 1);
        if ($action !== self::ANY && !Segment::isName($action)) {
            throw new ScopeError("`{$action}` is not a name");
        }
        $path = array_map(Segment::parse(...), $steps);
        return $path === [] ? throw new ScopeError('a scope needs a resource') : new self($path, $action);
    }

    /**
     * Whether it allows everything `$other` allows: its action the same or
     * `$*`, its path matching step by step (`*` in the middle: any one step;
     * at the end: anything below, itself included).
     */
    public function covers(self $other): bool
    {
        if ($this->action !== self::ANY && $this->action !== $other->action) {
            return false;
        }
        $last = count($this->path) - 1;
        foreach ($this->path as $at => $step) {
            if ($step->name === self::ANY && $at === $last) {
                return true;
            }
            if (!isset($other->path[$at]) || !$step->covers($other->path[$at])) {
                return false;
            }
        }
        return count($this->path) === count($other->path);
    }

    /**
     * Rust's order: by path (step by step, a shorter one first), then by
     * action (a name before `*`).
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
        $any = ($a->action === self::ANY) <=> ($b->action === self::ANY);
        return $any !== 0 ? $any : strcmp($a->action, $b->action) <=> 0;
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
        return implode('', array_map(static fn(Segment $step): string => "{$step}.", $this->path)) . '$' . $this->action;
    }
}
