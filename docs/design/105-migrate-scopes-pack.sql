-- Packed scopes: a shared prefix may take a scope's whole path (`*` is
-- the prefix itself, `*:read` read only), and a group of one scope is its
-- text, not a list: `{"com":["hashinbox"]}` is `{"com.hashinbox":"*"}`,
-- `{"a":["b:read","b.c"]}` is `{"a.b":["*:read","c"]}`. Rewrites every
-- stored packed scopes column (`user.scopes`, `user_key.scopes`; nothing
-- else stores them) to what `Scopes::pack` now writes. The server reads
-- both forms, so it is optional; run once, with the server stopped, after
-- a backup:
--
--   sqlite3 hashinbox.db < docs/design/105-migrate-scopes-pack.sql
--
-- Old pack kept one step after each prefix; the new one takes it too when
-- that step is a group's shortest scope, a plain name (`s`, `s:read`) the
-- group's other scopes go on from (`s.…`). Under the empty prefix each
-- such name starts its own group. Run twice, it changes nothing more.
-- Sessions issued before keep working. No table changes.

BEGIN;

CREATE TEMP TABLE pack_item AS
SELECT 'user' AS owner, held.id, packed.key AS prefix, item.key AS at, item.value AS text
    FROM "user" held, json_each(held.scopes) packed,
        json_each(CASE packed.type WHEN 'array' THEN packed.value
            ELSE json_array(packed.value) END) item
UNION ALL
SELECT 'user_key', held.id, packed.key, item.key, item.value
    FROM user_key held, json_each(held.scopes) packed,
        json_each(CASE packed.type WHEN 'array' THEN packed.value
            ELSE json_array(packed.value) END) item;

-- A scope one plain step long after its prefix: its step.
CREATE TEMP TABLE pack_step AS
SELECT owner, id, prefix, at,
        CASE WHEN text LIKE '%:read' THEN substr(text, 1, length(text) - 5) ELSE text END AS step
    FROM pack_item
    WHERE instr(text, '.') = 0 AND instr(text, '[') = 0 AND text NOT LIKE '*%';

-- Each step the prefix takes: under the empty prefix every one, else the
-- one every other scope of the group goes on from.
CREATE TEMP TABLE pack_taken AS
SELECT owner, id, prefix, at, step FROM pack_step candidate
    WHERE prefix = '' OR NOT EXISTS (
        SELECT 1 FROM pack_item other
            WHERE other.owner = candidate.owner AND other.id = candidate.id
                AND other.prefix = candidate.prefix AND other.at != candidate.at
                AND substr(other.text, 1, length(candidate.step) + 1) != candidate.step || '.');

CREATE TEMP TABLE pack_new AS
SELECT item.owner, item.id, item.at,
        CASE WHEN taken.step IS NULL THEN item.prefix
             WHEN item.prefix = '' THEN taken.step
             ELSE item.prefix || '.' || taken.step END AS prefix,
        CASE WHEN taken.step IS NULL THEN item.text
             WHEN taken.at = item.at THEN '*' || substr(item.text, length(taken.step) + 1)
             ELSE substr(item.text, length(taken.step) + 2) END AS text
    FROM pack_item item
    LEFT JOIN pack_taken taken
        ON taken.owner = item.owner AND taken.id = item.id AND taken.prefix = item.prefix
            AND (taken.at = item.at
                OR substr(item.text, 1, length(taken.step) + 1) = taken.step || '.');

CREATE TEMP TABLE pack_group AS
SELECT owner, id, prefix,
        CASE count(*) WHEN 1 THEN json_quote(min(text))
            ELSE json_group_array(text) END AS scopes
    FROM (SELECT * FROM pack_new ORDER BY owner, id, prefix, at)
    GROUP BY owner, id, prefix;

CREATE TEMP TABLE pack_row AS
SELECT owner, id, json_group_object(prefix, json(scopes)) AS scopes
    FROM (SELECT * FROM pack_group ORDER BY owner, id, prefix)
    GROUP BY owner, id;

UPDATE "user" SET scopes = (SELECT scopes FROM pack_row WHERE owner = 'user' AND pack_row.id = "user".id)
    WHERE id IN (SELECT id FROM pack_row WHERE owner = 'user');
UPDATE user_key SET scopes = (SELECT scopes FROM pack_row WHERE owner = 'user_key' AND pack_row.id = user_key.id)
    WHERE id IN (SELECT id FROM pack_row WHERE owner = 'user_key');

DROP TABLE pack_item;
DROP TABLE pack_step;
DROP TABLE pack_taken;
DROP TABLE pack_new;
DROP TABLE pack_group;
DROP TABLE pack_row;

COMMIT;
