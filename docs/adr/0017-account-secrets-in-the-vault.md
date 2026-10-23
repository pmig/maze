# ADR-017: Account secrets in the vault

- **Status:** Accepted. Supersedes the "Secrets" bullet of PLAN §4.6 (account secrets
  as OS keychain items via `keyring`); [ADR-012](0012-encryption-at-rest.md) stands.
- **Date:** 2026-10-23

## Context

PLAN §4.6 kept every account secret as its own OS keychain item: the IMAP password,
the OAuth refresh token and a separate SMTP password, service `mazemail`, account
`<account-uuid>/<kind>`. The storage master key ([ADR-012](0012-encryption-at-rest.md),
PLAN §4.17) is one more item.

On macOS each login-keychain item has an access list that trusts the app that created
it, by its code signature. Any other signature gets a system prompt per item. Dev
builds are signed ad hoc and get a new signature on every build, so "Always Allow"
doesn't stick: with three accounts a launch showed six prompts, one per item. Release
builds hit the same wall after an update whose designated requirement changes, and
every secret is one more item to ask about.

The extra items buy little. Whoever can read the master key can decrypt every message,
draft and attachment, which is what an attacker wants; the mail account password adds
reach (the server), but the threat model (ADR-012) already treats the master key as
the thing to protect. Thunderbird draws the line the same way: saved logins are
encrypted under one key (the primary password), not stored one by one in the system
keychain.

## Decision

The master key is Maze's only keychain item (Secret Service item on Linux). Account
secrets live in the encrypted store: a table `account_secret` in `mazemail.db`
(SQLCipher, schema v5), keyed by the account's UUID and the secret's kind, deleted
with its account in the same transaction. `auth` keeps the `SecretStore` trait;
`mazemail_core::StoreSecretStore` implements it over the store, and the app and
headless mode use it by default.

Existing keychain items are moved once. Schema v5 lists every account that existed
before it (`legacy_secret_account`). When the core opens, before any account starts
syncing, it copies each listed account's items into the store (unless the store
already has the secret), deletes the items, and takes the account off the list. A
secret that can't be read or saved keeps the account listed, so the next start tries
again; an item that was copied but can't be deleted is logged and left alone. Logs
name items (`mazemail/<account-uuid>/<kind>`), never secrets.

Secrets are `Sensitive` in logs and debug output, zeroized when the store hands them
out, and never written in plaintext. `MAZEMAIL_INSECURE_DEV_SECRETS=1` still selects
the plain development file for scripted tests.

## Consequences

- A launch reads the keychain once, for the master key, however many accounts there
  are. Ad-hoc-signed dev builds still prompt once per rebuild; `script/dev-codesign`
  (run by `mise start`) signs them with a stable local identity when one exists, so
  "Always Allow" sticks (CONTRIBUTING.md).
- The first start after the upgrade shows one last round of prompts, one per existing
  item (possibly once to read and once to delete it). The release notes and
  `docs/security.md` say so.
- Account secrets inherit the master key's protection: on macOS a login-keychain item
  (data protection keychain once release signing provides the entitlement), on Linux
  an unlocked Secret Service collection. They lose their per-item ACL, which mattered
  only against a process that can read one keychain item but not another.
- Pre-migration backups (`backups/`) contain the secrets, encrypted with the same key.
  A recovery key (PLAN §4.17) restores them with the mail; "Start fresh" loses them
  with it, and the accounts ask to sign in again.
- `auth`'s keychain stores (`KeychainSecretStore`, `SecretServiceStore`) stay only for
  the move. They can go once no supported version can still have such items.
