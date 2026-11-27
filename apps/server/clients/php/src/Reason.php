<?php

declare(strict_types=1);

namespace Hashinbox;

/**
 * Hand-written: the reasons an error answers with, what a client acts on, as
 * `ApiError::$reason`.
 */
final class Reason
{
    public const WrongCredentials = 'wrong_credentials';
    public const TotpRequired = 'totp_required';
    public const SessionExpired = 'session_expired';
    public const MissingScope = 'missing_scope';
}
