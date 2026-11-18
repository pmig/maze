# Settings model

The structure of `settings.json`. The defaults, with a comment for every key, are in
[`assets/settings/default.jsonc`](../assets/settings/default.jsonc); the types and the JSON
Schema come from `crates/settings/src/content.rs`. Update this diagram with every new or
removed setting.

**Bold** keys can be overridden per account in `accounts.<name>`; `accounts.<name>` also
holds what only exists per account: its `aliases` and, for Microsoft accounts,
`shared_contacts`.

```mermaid
flowchart LR
  classDef acct fill:#e8f0ff,stroke:#5c78e2
  classDef sec fill:#f5f5f5,stroke:#999

  S(("settings.json"))

  subgraph LOOK["Look and layout"]
    theme["theme<br/>mode · light · dark"]:::sec
    font_size["font_size"]
    layout["layout"]
    keymap["keymap<br/>primary_modifier · show_on_buttons"]:::sec
  end

  subgraph READ["Reading"]
    thread_list["thread_list<br/>view · sort · threaded"]:::sec
    folder_pane["folder_pane<br/>modes · favorites"]:::sec
    reader["reader<br/>default_mode · tab_mode · <b>remote_content</b>"]:::sec
    favicons["favicons"]
    notifications["notifications<br/>enabled · content"]:::sec
  end

  subgraph WRITE["Writing"]
    compose["compose<br/><b>footer · support_maze · copy_sent_to_inbox</b>"]:::sec
    spellcheck["spellcheck"]
  end

  subgraph SYNC["Sync and accounts"]
    offline["<b>offline</b> · <b>offline_recent_days</b>"]
    sync_trash["<b>sync_trash</b>"]
    contacts["contacts<br/><b>sync</b> · sync_interval_minutes"]:::sec
    auth["auth<br/>google · microsoft (OAuth clients)"]:::sec
    accounts["accounts.&lt;name&gt;<br/>aliases · shared_contacts<br/>+ overrides (bold keys)"]:::acct
  end

  subgraph AI["Agents"]
    agent["agent<br/>default · session_profile · custom ·<br/>env · auto_update"]:::sec
    agent_sandbox["agent_sandbox<br/>enabled · allow_read · deny_read ·<br/>agent_paths · network_hosts"]:::sec
    maze_magic["maze_magic<br/>agent · prompt · include_thread"]:::sec
  end

  subgraph APP["Updates"]
    update["update<br/>auto · channel · install"]:::sec
  end

  S --- LOOK & READ & WRITE & SYNC & AI & APP
  accounts -.->|overrides| offline
  accounts -.->|overrides| sync_trash
  accounts -.->|overrides| contacts
  accounts -.->|overrides| compose
  accounts -.->|overrides| reader

```

Example:

```jsonc
{
  "theme": { "mode": "dark" },
  "compose": { "footer": "Acme Inc." },
  "accounts": {
    "Work": { "offline": "all", "remote_content": "never" },
    "Private": { "footer": "", "support_maze": false }
  }
}
```
