<?php

declare(strict_types=1);

namespace Hashinbox;

use CurlHandle;
use CurlMultiHandle;
use Generator;
use RuntimeException;

/**
 * Hand-written: the default `Transport`, on ext-curl's multi interface, so a
 * stream's body is handed out as it arrives (its write callback buffers each
 * chunk; reading drives the transfer). No redirects are followed.
 *
 * @internal
 */
final class CurlTransport implements Transport
{
    public function open(string $method, string $url, array $headers, ?string $body, float $timeout, bool $stream): Reply
    {
        $curl = curl_init($url);
        if ($curl === false || $method === '') {
            throw new RuntimeException("curl: cannot open {$url}");
        }
        $chunks = [];
        $headed = false;
        $lines = ['Expect:'];
        foreach ($headers as $name => $value) {
            $lines[] = "{$name}: {$value}";
        }
        $ms = (int) ceil($timeout * 1000);
        curl_setopt($curl, CURLOPT_CUSTOMREQUEST, $method);
        curl_setopt_array($curl, [
            CURLOPT_HTTPHEADER => $lines,
            CURLOPT_CONNECTTIMEOUT_MS => $ms,
            CURLOPT_TIMEOUT_MS => $stream ? 0 : $ms,
            CURLOPT_HEADERFUNCTION => static function (CurlHandle $curl, string $line) use (&$headed): int {
                $headed = $headed || trim($line) === '';
                return strlen($line);
            },
            CURLOPT_WRITEFUNCTION => static function (CurlHandle $curl, string $data) use (&$chunks): int {
                $chunks[] = $data;
                return strlen($data);
            },
        ]);
        if ($body !== null) {
            curl_setopt($curl, CURLOPT_POSTFIELDS, $body);
        }
        $multi = curl_multi_init();
        curl_multi_add_handle($multi, $curl);
        $done = false;
        while (!$headed && !$done) {
            $done = self::step($multi, $curl);
        }
        $status = curl_getinfo($curl, CURLINFO_RESPONSE_CODE);
        $read = static function () use (&$chunks, &$done, $multi, $curl): Generator {
            try {
                while (true) {
                    while ($chunks !== []) {
                        yield array_shift($chunks);
                    }
                    if ($done) {
                        return;
                    }
                    $done = self::step($multi, $curl);
                }
            } finally {
                curl_multi_remove_handle($multi, $curl);
                curl_multi_close($multi);
            }
        };
        return new Reply($status, $read());
    }

    /**
     * Drives the transfer once, waiting up to a second for data: whether it is
     * over.
     *
     * @throws RuntimeException it failed
     */
    private static function step(CurlMultiHandle $multi, CurlHandle $curl): bool
    {
        $active = 0;
        do {
            $code = curl_multi_exec($multi, $active);
        } while ($code === CURLM_CALL_MULTI_PERFORM);
        if ($code !== CURLM_OK) {
            throw new RuntimeException('curl: ' . curl_multi_strerror($code));
        }
        if ($active > 0) {
            if (curl_multi_select($multi, 1.0) === -1) {
                usleep(1000);
            }
            return false;
        }
        $info = curl_multi_info_read($multi);
        if (is_array($info) && $info['result'] !== CURLE_OK) {
            throw new RuntimeException('curl: ' . curl_error($curl));
        }
        return true;
    }
}
