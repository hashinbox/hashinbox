-- Design 096: scopes as a tree. Brings a database made by the previous
-- `apps/server/init.sql` (`100-migrate-1.sql` applied) to the scope grammar
-- the server now reads. Run once, with the server stopped, after a backup:
--
--   sqlite3 payments.db < docs/design/096-migrate-2.sql
--
-- Rewrites every stored scope (`user.scopes`, `user_key.scopes`; nothing
-- else stores one): `X.$read` is `X:read`; `X.$write`, `X.$*` and `X.*` are
-- `X`; `endpoints` is `networks.endpoints` and `networks.routes`,
-- `transactions` is `networks.transactions`, `keys` is `users.keys`.
-- `assets` and `markets:read` need no scope now and are dropped, as is any
-- scope it cannot map (another wildcard, another action, another
-- namespace): the last statement lists each one dropped. Sessions issued
-- before it no longer parse and answer 401: log in again. No table
-- changes.

BEGIN;

CREATE TEMP TABLE scope_old AS
SELECT 'user' AS owner, held.id, CASE WHEN packed.key = '' THEN item.value
        ELSE packed.key || '.' || item.value END AS text
    FROM "user" held, json_each(held.scopes) packed, json_each(packed.value) item
UNION ALL
SELECT 'user_key', held.id, CASE WHEN packed.key = '' THEN item.value
        ELSE packed.key || '.' || item.value END
    FROM user_key held, json_each(held.scopes) packed, json_each(packed.value) item;

CREATE TEMP TABLE scope_new AS
WITH level AS (
    SELECT owner, id, text,
        CASE WHEN text LIKE '%.$read' THEN substr(text, 1, length(text) - 6)
             WHEN text LIKE '%.$write' THEN substr(text, 1, length(text) - 7)
             WHEN text LIKE '%.$*' THEN substr(text, 1, length(text) - 3) END AS base,
        CASE WHEN text LIKE '%.$read' THEN ':read' ELSE '' END AS read
    FROM scope_old
), path AS (
    SELECT owner, id, text, read,
        CASE WHEN base LIKE '%.*' THEN substr(base, 1, length(base) - 2) ELSE base END AS base
    FROM level
), rest AS (
    SELECT owner, id, text, read,
        CASE WHEN base = 'com.hashinbox' THEN ''
             WHEN base LIKE 'com.hashinbox.%' AND instr(base, '*') = 0
                 THEN substr(base, 15) END AS rest
    FROM path
), step AS (
    SELECT owner, id, text, read, rest,
        substr(rest, 1, min(instr(rest || '.', '.'), instr(rest || '[', '[')) - 1) AS first,
        substr(rest, min(instr(rest || '.', '.'), instr(rest || '[', '['))) AS tail
    FROM rest WHERE rest IS NOT NULL
), renamed (owner, id, text, read, rest) AS (
    SELECT owner, id, text, read, rest FROM step
        WHERE first IN ('', 'wallets', 'networks', 'triggers', 'jobs', 'logs', 'stats',
            'users', 'settings', 'recovery')
            OR (first = 'markets' AND read = '')
    UNION ALL SELECT owner, id, text, read, 'networks.endpoints' || tail FROM step
        WHERE first = 'endpoints'
    UNION ALL SELECT owner, id, text, read, 'networks.routes' || tail FROM step
        WHERE first = 'endpoints'
    UNION ALL SELECT owner, id, text, read, 'networks.transactions' || tail FROM step
        WHERE first = 'transactions'
    UNION ALL SELECT owner, id, text, read, 'users.keys' || tail FROM step
        WHERE first = 'keys'
)
SELECT owner, id, text,
    CASE WHEN rest = '' THEN 'com.hashinbox' ELSE 'com.hashinbox.' || rest END || read AS scope
FROM renamed;

-- Packed as the server reads it: under `com` when the root is held, else
-- under `com.hashinbox`.
CREATE TEMP TABLE scope_packed AS
SELECT owner, id,
    CASE WHEN max(scope IN ('com.hashinbox', 'com.hashinbox:read'))
        THEN json_object('com', json_group_array(DISTINCT substr(scope, 5)))
        ELSE json_object('com.hashinbox', json_group_array(DISTINCT substr(scope, 15)))
    END AS scopes
FROM scope_new GROUP BY owner, id;

UPDATE "user" SET scopes = coalesce(
    (SELECT scopes FROM scope_packed WHERE owner = 'user' AND scope_packed.id = "user".id),
    '{}');
UPDATE user_key SET scopes = coalesce(
    (SELECT scopes FROM scope_packed WHERE owner = 'user_key' AND scope_packed.id = user_key.id),
    '{}');

SELECT 'dropped', owner, id, text FROM scope_old
WHERE (owner, id, text) NOT IN (SELECT owner, id, text FROM scope_new);

COMMIT;
