<?php

declare(strict_types=1);

namespace Hashinbox;

/**
 * Hand-written: an operation's static description. The generated
 * `Operations.php` declares one per operation (`WALLET_GET`).
 */
final readonly class Operation
{
    /**
     * @param string $path The path template: `/v1/wallets/{id}`.
     * @param list<string> $templates The scope templates: `com.hashinbox.wallets[{id}]:read`.
     */
    public function __construct(
        public string $id,
        public string $method,
        public string $path,
        public array $templates = [],
    ) {}

    /**
     * The scopes it needs for `$params` (its path parameters), each hole escaped.
     *
     * @param array<string, string> $params
     * @return list<string>
     */
    public function scopes(array $params = []): array
    {
        return array_map(static fn(string $template): string => Scope::fill($template, $params), $this->templates);
    }
}
