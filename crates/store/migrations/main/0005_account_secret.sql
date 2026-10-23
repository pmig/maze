-- Maze store, schema v5: account secrets in the encrypted database (ADR-017).
--
-- IMAP passwords, OAuth refresh tokens and SMTP passwords used to be separate items
-- in the OS keychain (service `mazemail`, account `<account-uuid>/<kind>`), one system
-- prompt each. They now live here, encrypted like everything in this database
-- (SQLCipher, under the master key that is Maze's only keychain item). `kind` is the
-- `auth` crate's name (`password`, `oauth-refresh`, `smtp-password`). Deleting an
-- account deletes its secrets in the same transaction.
--
-- `legacy_secret_account` lists the accounts whose secrets may still be in the OS
-- keychain: every account that exists when this migration runs. At startup the core
-- moves their keychain items into `account_secret`, deletes the items and removes the
-- account from this list. Accounts added later never had keychain items.

CREATE TABLE account_secret (
  account_uuid TEXT NOT NULL REFERENCES account (uuid) ON DELETE CASCADE,
  kind         TEXT NOT NULL,
  secret       TEXT NOT NULL,
  updated      INTEGER NOT NULL,
  PRIMARY KEY (account_uuid, kind)
) WITHOUT ROWID;

CREATE TABLE legacy_secret_account (
  account_uuid TEXT PRIMARY KEY REFERENCES account (uuid) ON DELETE CASCADE
) WITHOUT ROWID;

INSERT INTO legacy_secret_account (account_uuid) SELECT uuid FROM account WHERE uuid <> '';
