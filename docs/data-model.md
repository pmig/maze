# Data model

What Maze stores, as of the v1 schema (`crates/store/migrations/main/0001_schema_v1.sql`,
33 tables) and the rules engine's two tables (`0002_rules.sql`) plus the send log (`crates/store/migrations/sendlog/`). Everything is in
SQLCipher databases encrypted by the vault ([ADR-017](adr/0017-account-secrets-in-the-vault.md));
message bodies, queued messages and agent transcripts are encrypted blobs referenced
from `blob`.

Arrows are foreign keys (child to parent). Dashed arrows are references by value without
a foreign key. Update this diagram with every schema change.

```mermaid
flowchart LR
  classDef other fill:#f3f0ff,stroke:#8b7bd8

  subgraph ACC["Accounts and identities"]
    account["<b>account</b><br/>name, kind (imap, gmail, m365),<br/>config_json, sync_state_json, uuid"]
    account_secret["<b>account_secret</b><br/>passwords, OAuth tokens"]
    identity["<b>identity</b><br/>email, display_name, reply_to,<br/>sent/drafts/archive folder"]
    smtp_server["<b>smtp_server</b>"]
    signature["<b>signature</b>"]
  end

  subgraph MAIL["Mail"]
    folder["<b>folder</b><br/>remote_name, role, uidvalidity,<br/>modseq, unread, total"]
    thread["<b>thread</b><br/>subject, last_date, counts,<br/>participants"]
    folder_thread["<b>folder_thread</b><br/>per-folder thread counts"]
    message["<b>message</b><br/>headers, snippet, bodystructure,<br/>gm_msgid, auth_json"]
    message_location["<b>message_location</b><br/>uid, flags, keywords"]
    message_ref["<b>message_ref</b><br/>References / In-Reply-To"]
    message_tag["<b>message_tag</b>"]
    tag["<b>tag</b>"]
    blob["<b>blob</b><br/>encrypted body, refcount"]
  end

  subgraph SEND["Writing and sending"]
    draft["<b>draft</b><br/>to/cc/bcc, subject, body (Markdown),<br/>name, support_maze"]
    outbox["<b>outbox</b><br/>queued messages"]
    pending_op["<b>pending_op</b><br/>queued IMAP changes, appends"]
    sent["<b>sent</b> (sendlog.db)<br/>what the server accepted"]:::other
  end

  subgraph PEOPLE["Contacts and domains"]
    contact["<b>contact</b><br/>one row per address, book"]
    contact_source["<b>contact_source</b><br/>synced book state"]
    contact_person["<b>contact_person</b><br/>server copy (Google)"]
    contact_photo["<b>contact_photo</b>"]
    domain["<b>domain</b><br/>counts, favicon state"]
    message_domain["<b>message_domain</b>"]
    remote_content_sender["<b>remote_content_sender</b>"]
  end

  subgraph AGENTS["Agents and API"]
    api_token["<b>api_token</b><br/>hash, profile, scope"]
    proposal["<b>proposal</b>"]
    proposal_item["<b>proposal_item</b><br/>command_json"]
    audit_log["<b>audit_log</b>"]
    agent_thread["<b>agent_thread</b><br/>ACP session, transcript"]
    agent_secret["<b>agent_secret</b><br/>API keys"]
  end

  subgraph USER["Your lists"]
    rule["<b>rule</b><br/>filter rules"]
    rule_cursor["<b>rule_cursor</b><br/>last UID the rules saw"]
    rule_message["<b>rule_message</b><br/>messages the rules processed"]
    saved_search["<b>saved_search</b>"]
    spelling_word["<b>spelling_word</b>"]
  end

  account_secret --> account
  identity --> account
  identity --> smtp_server
  identity --> signature
  identity -->|sent / drafts / archive| folder
  folder --> account
  folder -->|parent| folder
  thread --> account
  folder_thread --> folder
  folder_thread --> thread
  message --> account
  message --> thread
  message --> blob
  message -->|identity_hint| identity
  message_location --> message
  message_location --> folder
  message_ref --> message
  message_tag --> message
  message_tag --> tag
  draft --> identity
  outbox --> identity
  outbox --> blob
  pending_op --> account
  pending_op --> blob
  sent -.->|rfc_message_id| outbox
  contact_source --> account
  contact_person --> contact_source
  contact -.->|book name| contact_source
  message_domain --> message
  message_domain --> domain
  proposal --> api_token
  proposal_item --> proposal
  agent_thread --> thread
  agent_thread -->|transcript| blob
  rule --> account
  rule_cursor --> folder
  rule_message --> message
```

## Notes

- **`message` is per account**: one row per message and account, with its places in
  `message_location` (one row per folder it is in, with the IMAP UID and flags). A Gmail
  message with several labels is one `message` with several locations.
- **Threads** are per account too; `folder_thread` keeps the per-folder counts the message
  list shows without scanning messages.
- **`blob`** counts its references (triggers on `message`, `outbox`, `pending_op` and
  `agent_thread`), so unreferenced bodies can be removed.
- **Contacts**: `contact` has one row per address (the book is a string such as
  `google:<account uuid>`); `contact_source` and `contact_person` are the sync state and the
  server copy of synced books ([ADR-024](adr/0024-server-owned-contacts.md)).
- **Rules** ([ADR-029](adr/0029-message-rules.md)) keep their conditions and actions as
  JSON in `rule`. `rule_cursor` remembers, per watched folder and UIDVALIDITY, the
  highest UID the arrival rules have seen; `rule_message` the messages they processed,
  so each is filtered once.
- **The send log** (`sendlog.db`, table `sent`) records every message a server accepted,
  so a crash while sending never sends it twice.
