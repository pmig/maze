-- Maze store, schema v2: a stable UUID per account.
--
-- The `auth` crate keys an account's keychain secrets by it
-- (`mazemail/<account-uuid>/<kind>`), so it must never change and never be reused.
-- The store generates it (UUID v4, lowercase hyphenated text) when it creates an
-- account. `ADD COLUMN` can't add a UNIQUE constraint or a non-constant default, hence
-- the empty default, the unique index and the triggers: a row inserted without a UUID
-- gets one right away, and a UUID once set can't be changed.

ALTER TABLE account ADD COLUMN uuid TEXT NOT NULL DEFAULT '';

-- Random v4 UUIDs for existing accounts (`randomblob` is evaluated per row).
UPDATE account SET uuid =
  lower(hex(randomblob(4))) || '-' || lower(hex(randomblob(2))) || '-4' ||
  substr(lower(hex(randomblob(2))), 2) || '-' ||
  substr('89ab', 1 + (abs(random()) % 4), 1) || substr(lower(hex(randomblob(2))), 2) || '-' ||
  lower(hex(randomblob(6)));

CREATE UNIQUE INDEX account_uuid ON account (uuid);

CREATE TRIGGER account_uuid_default AFTER INSERT ON account WHEN NEW.uuid = ''
BEGIN
  UPDATE account SET uuid =
    lower(hex(randomblob(4))) || '-' || lower(hex(randomblob(2))) || '-4' ||
    substr(lower(hex(randomblob(2))), 2) || '-' ||
    substr('89ab', 1 + (abs(random()) % 4), 1) || substr(lower(hex(randomblob(2))), 2) || '-' ||
    lower(hex(randomblob(6)))
  WHERE id = NEW.id;
END;

CREATE TRIGGER account_uuid_immutable BEFORE UPDATE OF uuid ON account
WHEN OLD.uuid <> '' AND NEW.uuid IS NOT OLD.uuid
BEGIN
  SELECT RAISE(ABORT, 'account uuid is immutable');
END;
