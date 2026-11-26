<?php

declare(strict_types=1);

namespace Hashinbox;

use Closure;
use JsonException;
use RuntimeException;
use UnexpectedValueException;

/**
 * Hand-written: the hashinbox API over ext-curl, the `$client` of every
 * generated operation (`Operations.php`), with the session it sends. cligen
 * (via `apps/server/build.rs`) writes `Models.php` and `Operations.php` beside
 * this; it never touches this file.
 *
 * A session is always a JWT; a key's secret never goes over the wire.
 *
 * - `$session`: a JWT session to send (a login's, or one a backend issued).
 * - `$secret`: a key; the client signs its own sessions with it: `$scopes`, or
 *   the key's own (read once with a scopeless session from `GET /v1/session`),
 *   lasting `$ttl` seconds, signed anew within 60 s of expiry or after a 401
 *   `session_expired` (retried once). This machine's clock must be within
 *   60 s of the server's: keep it on NTP.
 * - `$timeout`: seconds a call may take (a stream: to connect).
 * - `$backoff`, `$backoffMax`: seconds a stream waits before reconnecting
 *   after an error, doubling from the first up to the second.
 * - `$checkScopes`: refuse locally (`MissingScope`) a call the session's
 *   scopes do not cover.
 */
final class Client
{
    /** How long before its `exp` a self-signed session is replaced, seconds. */
    public const REFRESH = 60;

    private readonly string $baseUrl;

    private readonly Transport $transport;

    public function __construct(
        string $baseUrl,
        private ?Session $session = null,
        private readonly ?Secret $secret = null,
        private ?Scopes $scopes = null,
        private readonly int $ttl = 600,
        private readonly float $timeout = 30.0,
        private readonly bool $checkScopes = true,
        ?Transport $transport = null,
        private readonly float $backoff = 0.5,
        private readonly float $backoffMax = 8.0,
    ) {
        $this->baseUrl = rtrim($baseUrl, '/');
        $this->transport = $transport ?? new CurlTransport();
    }

    /**
     * The session sent; null before a secret signs its first.
     */
    public function session(): ?Session
    {
        return $this->session;
    }

    public function setSession(?Session $session): void
    {
        $this->session = $session;
    }

    /**
     * `$request` sent with the session: its 2xx body decoded (objects as
     * arrays; null when empty).
     *
     * @throws ApiError a non-2xx answer
     * @throws MissingScope a scope the session lacks, unsent
     * @throws RuntimeException the transport failed
     * @throws JsonException a body that is not JSON
     */
    public function send(Request $request): mixed
    {
        $body = $this->exchange($request)->body();
        return $body === '' ? null : Json::decode($body);
    }

    /**
     * `$request`'s server-sent events, as read, or each as `$decode` makes it
     * (null: skipped): a handle reconnecting across the server's windows
     * until `stop()` (see `Stream`); sent on the first iteration. A stream's
     * reads wait as long as it takes.
     *
     * @template T
     * @param (Closure(RawEvent): (T|null))|null $decode
     * @return ($decode is null ? Stream<RawEvent> : Stream<T>)
     */
    public function open(Request $request, ?Closure $decode = null): Stream
    {
        $connect = fn(?string $cursor): Reply => $this->exchange($request, stream: true, cursor: $cursor);
        if ($decode === null) {
            return new Stream($connect, static fn(RawEvent $raw): RawEvent => $raw, $this->backoff, $this->backoffMax);
        }
        return new Stream($connect, $decode, $this->backoff, $this->backoffMax);
    }

    /**
     * `$request`'s server-sent events, their data decoded from JSON: a handle,
     * as `open`'s.
     *
     * @return Stream<Event<mixed>>
     */
    public function stream(Request $request): Stream
    {
        return $this->open($request, static fn(RawEvent $raw): Event => new Event($raw->event, Json::decode($raw->data), $raw->id));
    }

    /**
     * Logs in (no `Authorization` sent) and keeps the session. A wrong email,
     * password, passphrase or code throws an `ApiError` with reason
     * `wrong_credentials`; a missing code, `totp_required` (ask it, call again
     * with `$totp`).
     *
     * @throws ApiError
     */
    public function login(string $email, ?string $password = null, ?string $passphrase = null, ?string $totp = null): IssuedSession
    {
        $body = new CreateSessionRequest(email: $email, passphrase: $passphrase, password: $password, totp: $totp);
        $reply = $this->exchange(new SessionCreateRequest($body), anonymous: true);
        $issued = IssuedSession::fromJson(Json::object(Json::decode($reply->body())));
        $this->session = Session::parse($issued->token);
        return $issued;
    }

    /**
     * Forgets the session; with `$all`, first has the server refuse every
     * session its issuer issued until now.
     *
     * @throws ApiError
     */
    public function logout(bool $all = false): void
    {
        try {
            if ($all) {
                $this->send(new SessionDeleteRequest(all: true));
            }
        } finally {
            $this->session = null;
        }
    }

    /**
     * `$request` sent with the session (signed anew once on a
     * `session_expired` when a secret signs them): its 2xx reply.
     *
     * @throws ApiError
     */
    private function exchange(Request $request, bool $stream = false, bool $anonymous = false, ?string $cursor = null): Reply
    {
        $retry = $this->secret !== null && !$anonymous;
        while (true) {
            $session = $anonymous ? null : $this->current();
            if ($session !== null && $this->checkScopes) {
                foreach ($request->operation()->scopes($request->params()) as $scope) {
                    if (!$session->scopes->covers($scope)) {
                        throw new MissingScope($request->operation()->id, $scope);
                    }
                }
            }
            $reply = $this->fetch($request, $session, $stream, $cursor);
            if ($reply->ok()) {
                return $reply;
            }
            $error = ApiError::read($reply->status, $reply->body());
            if (!$retry || $error->status !== 401 || $error->reason !== 'session_expired') {
                throw $error;
            }
            $retry = false;
            $this->session = null;
        }
    }

    /**
     * One request: its path filled (each parameter URL-escaped), its query,
     * its JSON body and the bearer.
     */
    private function fetch(Request $request, ?Session $session, bool $stream, ?string $cursor = null): Reply
    {
        $operation = $request->operation();
        $url = $this->baseUrl . Scope::fill($operation->path, $request->params(), rawurlencode(...));
        $query = $request->query();
        if ($cursor !== null) {
            $query = array_values(array_filter($query, static fn(array $pair): bool => $pair[0] !== 'cursor'));
            $query[] = ['cursor', $cursor];
        }
        $pairs = array_map(
            static fn(array $pair): string => rawurlencode($pair[0]) . '=' . rawurlencode($pair[1]),
            $query,
        );
        if ($pairs !== []) {
            $url .= '?' . implode('&', $pairs);
        }
        $headers = ['Accept' => $stream ? 'text/event-stream' : 'application/json'];
        if ($session !== null) {
            $headers['Authorization'] = "Bearer {$session->token}";
        }
        $content = $request->content();
        if ($content !== null) {
            $headers['Content-Type'] = 'application/json';
        }
        return $this->transport->open($operation->method, $url, $headers, $content, $this->timeout, $stream);
    }

    /**
     * The session to send: the one kept, or one the secret signs when it lapses
     * within a minute.
     */
    private function current(): ?Session
    {
        if ($this->secret === null) {
            return $this->session;
        }
        if ($this->session === null || $this->session->expiresAt - time() <= self::REFRESH) {
            $this->session = $this->sign($this->secret);
        }
        return $this->session;
    }

    /**
     * A new session of `$secret`: the scopes asked, or the key's own, read once
     * with a scopeless session.
     *
     * @throws ApiError
     */
    private function sign(Secret $secret): Session
    {
        if ($this->scopes === null) {
            $reply = $this->fetch(new SessionGetRequest(), $secret->issue(ttl: self::REFRESH), false);
            $body = $reply->body();
            if (!$reply->ok()) {
                throw ApiError::read($reply->status, $body);
            }
            $read = SessionRead::fromJson(Json::object(Json::decode($body)));
            if ($read->key === null) {
                throw new UnexpectedValueException('GET /v1/session answered no key for a key session');
            }
            $this->scopes = Scopes::parse($read->key->scopes);
        }
        return $secret->issue($this->scopes, ttl: $this->ttl);
    }
}
