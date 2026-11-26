<?php

declare(strict_types=1);

namespace Hashinbox;

use InvalidArgumentException;
use JsonException;
use SensitiveParameter;

/**
 * Hand-written: a session JWT (design 096), decoded but not verified: only its
 * issuer can check it, and the server does.
 */
final readonly class Session
{
    /**
     * @param string $token The JWT, sent as `Authorization: Bearer …`.
     * @param string $issuer `iss` (= the header's `kid`): `user/{id}`, or a key's `hip/…`.
     * @param ?string $subject `sub`: a user's id, or who a key's session is for.
     * @param Scopes $scopes What it may do, unpacked.
     * @param int $issuedAt `iat`, Unix seconds.
     * @param int $expiresAt `exp`, Unix seconds.
     * @param list<Amr> $amr How its login proved who it is; empty for a key's.
     */
    public function __construct(
        #[SensitiveParameter]
        public string $token,
        public string $issuer,
        public ?string $subject,
        public Scopes $scopes,
        public int $issuedAt,
        public int $expiresAt,
        public array $amr = [],
    ) {}

    /**
     * The session `$token` is: its claims decoded, its signature not checked.
     *
     * @throws InvalidArgumentException when it is not a session JWT
     */
    public static function parse(#[SensitiveParameter] string $token): self
    {
        $parts = explode('.', $token);
        if (count($parts) !== 3) {
            throw new InvalidArgumentException('not a JWT');
        }
        $header = self::part($parts[0]);
        $claims = self::part($parts[1]);
        if (($header['alg'] ?? null) !== 'HS256') {
            throw new InvalidArgumentException('its alg is not HS256');
        }
        $issuer = $claims['iss'] ?? null;
        if (!is_string($issuer) || ($header['kid'] ?? null) !== $issuer) {
            throw new InvalidArgumentException('its header does not name its issuer');
        }
        $iat = $claims['iat'] ?? null;
        $exp = $claims['exp'] ?? null;
        if (!is_int($iat) || !is_int($exp)) {
            throw new InvalidArgumentException('it has no times');
        }
        $scopes = $claims['scopes'] ?? [];
        $amr = $claims['amr'] ?? [];
        if (!is_array($scopes) || !is_array($amr)) {
            throw new InvalidArgumentException('its scopes or amr are not JSON objects');
        }
        $subject = $claims['sub'] ?? null;
        return new self(
            token: $token,
            issuer: $issuer,
            subject: is_string($subject) ? $subject : null,
            scopes: Scopes::parse($scopes),
            issuedAt: $iat,
            expiresAt: $exp,
            amr: array_values(array_map(static fn(mixed $method): Amr => Amr::from(Json::string($method)), $amr)),
        );
    }

    /**
     * Whether it has expired at `$now` (Unix seconds; the clock's by default).
     */
    public function expired(?int $now = null): bool
    {
        return $this->expiresAt <= ($now ?? time());
    }

    /**
     * Whether it holds every scope `$operation` needs for `$params` (its path
     * parameters).
     *
     * @param array<string, string> $params
     */
    public function can(Operation $operation, array $params = []): bool
    {
        foreach ($operation->scopes($params) as $scope) {
            if (!$this->scopes->covers($scope)) {
                return false;
            }
        }
        return true;
    }

    /**
     * @return array<string, mixed> a JWT part's JSON object
     */
    private static function part(string $part): array
    {
        $raw = base64_decode(strtr($part, '-_', '+/') . str_repeat('=', (4 - strlen($part) % 4) % 4), true);
        try {
            $value = $raw === false ? null : Json::decode($raw);
        } catch (JsonException) {
            $value = null;
        }
        return is_array($value) ? Json::object($value) : throw new InvalidArgumentException('not a JWT');
    }
}
