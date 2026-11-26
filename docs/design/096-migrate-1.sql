-- Design 096: users, keys, sessions and scopes. Brings a database made by
-- the previous `apps/server/init.sql` (`095-migrate-1.sql` applied) to the
-- new one. Run once, with the server stopped, after a backup:
--
--   sqlite3 payments.db < docs/design/096-migrate-1.sql
--
-- Adds the `user`, `user_key` and `user_2fa` tables. No user exists after
-- it: `GET /v1/version` answers `missing_root`, and the first
-- `POST /v1/users` creates the root user. The env bearer token
-- (`PAYMENT_BEARER_TOKEN_ENV`) is gone.

BEGIN;

CREATE TABLE "user" (
    id             TEXT    PRIMARY KEY,
    email          TEXT    NOT NULL UNIQUE,
    name           TEXT    NOT NULL,
    password_hash  TEXT,
    scopes         TEXT    NOT NULL CHECK (json_valid(scopes)),
    session_secret BLOB    NOT NULL,
    created_at     INTEGER NOT NULL,
    invalidated_at INTEGER,
    disabled_at    INTEGER
);

CREATE TABLE user_key (
    id             TEXT    PRIMARY KEY,
    user_id        TEXT    NOT NULL REFERENCES "user" (id) ON DELETE CASCADE,
    name           TEXT    NOT NULL,
    secret_hash    BLOB    NOT NULL UNIQUE,
    secret         TEXT    NOT NULL,
    scopes         TEXT    NOT NULL CHECK (json_valid(scopes)),
    created_at     INTEGER NOT NULL,
    expires_at     INTEGER,
    invalidated_at INTEGER,
    revoked_at     INTEGER,
    last_used_at   INTEGER
);
CREATE INDEX user_key_user ON user_key (user_id);

CREATE TABLE user_2fa (
    user_id      TEXT    PRIMARY KEY REFERENCES "user" (id) ON DELETE CASCADE,
    secret       BLOB    NOT NULL,
    confirmed_at INTEGER,
    last_step    INTEGER,
    created_at   INTEGER NOT NULL
);

COMMIT;
