<?php

declare(strict_types=1);

namespace Hashinbox;

use InvalidArgumentException;
use SensitiveParameter;

/**
 * Hand-written: a key's secret (design 096) and the sessions it signs. The
 * secret never goes over the wire: only the JWTs it signs do.
 *
 * Signing needs this machine's clock roughly right: the server refuses a
 * session whose `iat` is more than 60 s ahead of its own clock, or whose `exp`
 * is past. Keep it on NTP.
 */
final class Secret
{
    /** The longest a session lasts: 24 hours, in seconds. */
    public const MAX_TTL = 86400;

    private const ALPHABET = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz';

    private function __construct(
        public readonly string $id,
        #[SensitiveParameter]
        private readonly string $value,
    ) {}

    /**
     * The key `$id` (`hip/…`) and `$value` (`his/…`) are.
     *
     * @throws InvalidArgumentException when `$value` is not a `his/…` secret or fails its checksum (a typo)
     */
    public static function parse(string $id, #[SensitiveParameter] string $value): self
    {
        if (!str_starts_with($id, 'hip/') || $id === 'hip/') {
            throw new InvalidArgumentException('a key id is `hip/…`');
        }
        $raw = str_starts_with($value, 'his/') ? self::base58(substr($value, 4)) : null;
        if ($raw === null || strlen($raw) !== 36) {
            throw new InvalidArgumentException('not a `his/…` secret');
        }
        if (hash('crc32b', substr($raw, 0, 32), true) !== substr($raw, 32)) {
            throw new InvalidArgumentException("the secret's checksum does not match: a typo?");
        }
        return new self($id, $value);
    }

    /**
     * A session of the key, HS256-signed here (`iss` = `kid` = its id, the
     * `his/…` text the key): `$scopes` (none when null; the server refuses any
     * the key does not cover), for `$subject`, lasting `$ttl` seconds (1 to
     * `MAX_TTL`) from `$issuedAt` (the clock's by default). Its header and
     * claims are written as the server writes them, byte for byte.
     *
     * @throws InvalidArgumentException a ttl out of range
     */
    public function issue(?Scopes $scopes = null, ?string $subject = null, int $ttl = 600, ?int $issuedAt = null): Session
    {
        if ($ttl < 1 || $ttl > self::MAX_TTL) {
            throw new InvalidArgumentException('ttl is 1 to ' . self::MAX_TTL . ' seconds');
        }
        $iat = $issuedAt ?? time();
        $claims = ['iss' => $this->id];
        if ($subject !== null) {
            $claims['sub'] = $subject;
        }
        $claims += ['iat' => $iat, 'exp' => $iat + $ttl, 'scopes' => (object) ($scopes ?? new Scopes())->pack()];
        $header = ['typ' => 'JWT', 'alg' => 'HS256', 'kid' => $this->id];
        $body = self::base64url(Json::dump($header)) . '.' . self::base64url(Json::dump($claims));
        return Session::parse($body . '.' . self::base64url(hash_hmac('sha256', $body, $this->value, true)));
    }

    /**
     * @return array{id: string} its id only
     */
    public function __debugInfo(): array
    {
        return ['id' => $this->id];
    }

    private static function base64url(string $raw): string
    {
        return rtrim(strtr(base64_encode($raw), '+/', '-_'), '=');
    }

    /**
     * The bytes base-58 `$text` is, each leading `1` a zero byte; null for a
     * character outside the alphabet.
     */
    private static function base58(string $text): ?string
    {
        $bytes = [];
        $length = strlen($text);
        for ($at = 0; $at < $length; $at++) {
            $carry = strpos(self::ALPHABET, $text[$at]);
            if ($carry === false) {
                return null;
            }
            for ($byte = count($bytes) - 1; $byte >= 0; $byte--) {
                $carry += $bytes[$byte] * 58;
                $bytes[$byte] = $carry & 0xff;
                $carry >>= 8;
            }
            for (; $carry > 0; $carry >>= 8) {
                array_unshift($bytes, $carry & 0xff);
            }
        }
        $zeros = $length - strlen(ltrim($text, '1'));
        return str_repeat("\0", $zeros) . pack('C*', ...$bytes);
    }
}
