# Changelog

## 0.1.0-alpha.1 (2026-10-30)


### ⚠ BREAKING CHANGES

* Maze no longer has Local Folders or local accounts, and mbox/Maildir imports no longer go into a local account. Updating removes empty Local Folders; Local Folders that still hold mail stay readable, searchable and exportable (never synced) until a later version removes them. Keep that mail with `mazemail export 'account:"Local Folders"' --to local-folders.mbox` and `mazemail import local-folders.mbox --account <account>`. `mazemail import` of an mbox file or Maildir now needs an IMAP account (`--account` unless there is only one) and uploads to the server.

### Features

* add IMAP, Gmail and Microsoft 365 accounts with live sync in the app and CLI ([846690c](https://github.com/pmig/maze/commit/846690c1a6b3680c0e932358bdb4dca0e94a1669))
* add mazemail binary skeleton with CLI entry point ([8e50f61](https://github.com/pmig/maze/commit/8e50f61720d0bf83d68ac7633ed3b1e95d72ca30))
* add release channels, settings, auto-updater core and release manifest xtask ([5d8dbd3](https://github.com/pmig/maze/commit/5d8dbd3bfcb87d618a4cdd990ba5d39a332d0324))
* add the in-app auto-updater with Restart to update, rollback and update notices ([be5f45f](https://github.com/pmig/maze/commit/be5f45f45397ee1288170f4fbae2697561766bdd))
* **agent_ui:** sign in to agents, pick models, auto-update, Zed-style panel ([a77adde](https://github.com/pmig/maze/commit/a77adde69555d46e6138d28bc3a55e3c841ceef0))
* **agent:** run ACP agents sandboxed in the agent column ([432c09a](https://github.com/pmig/maze/commit/432c09ad7c54a7a0e73ccf05884a7a1e3499c51b))
* apply the theme setting, with Zed themes from the themes folder ([8d14cbb](https://github.com/pmig/maze/commit/8d14cbbb6c991f25cc583b3cdf44cbe7edb9f864))
* **auth:** add OAuth2 PKCE sign-in for Google and Microsoft, token refresh and keychain secrets ([c0d0fb2](https://github.com/pmig/maze/commit/c0d0fb2f1ed956a3f2aa153ea1f06059564f5c4f))
* **auth:** keep account secrets in the encrypted store, one keychain item ([af2cb17](https://github.com/pmig/maze/commit/af2cb174e8dd799ff5ec6437359dcc571e6bd5e3))
* **composer:** make "Sent with Maze" a setting only ([3484e9d](https://github.com/pmig/maze/commit/3484e9dedae7c29663d674fc7cfbe666007b8d1b))
* **composer:** write and send mail ([64642e2](https://github.com/pmig/maze/commit/64642e28942f41f9c300a01e1dcb588f5bb8b7ab))
* **core:** add the CoreHandle facade with queries, search, commands, undo and events ([62965de](https://github.com/pmig/maze/commit/62965de52a0fe1f6a8dfcba436535dd5b661237d))
* disk usage and sync activity in the status bar, Microsoft tenant ([673c51e](https://github.com/pmig/maze/commit/673c51ecbaab5ffd5147c9d5b416a22b260f5b1f))
* edit settings.json in a tab, compose footer and "Sent with Maze" ([9432e32](https://github.com/pmig/maze/commit/9432e324c1fd9fa3a07ba20db622c7c399eb9744))
* go to folder on cmd-k, search on cmd-shift-f, Ctrl as cmd on Linux ([56b8135](https://github.com/pmig/maze/commit/56b8135a9fb444c126b1dfff447747159e54e166))
* **imap:** add the IMAP protocol layer with CONDSTORE, QRESYNC, UIDPLUS, Gmail and a fake server ([206562f](https://github.com/pmig/maze/commit/206562f01002b7c1f4fd8c8925cc5983434ca62d))
* **import:** add the shared ingest pipeline and mbox, Maildir and Thunderbird profile import ([2a8504e](https://github.com/pmig/maze/commit/2a8504e2fe513eb312247c910c188ebe13bd2c28))
* **local_api:** add the socket protocol, command registry, scoped tokens and approval flow ([0330e5a](https://github.com/pmig/maze/commit/0330e5a74a02efc7d9a83ac51eff5b4ad203b5d6))
* **mail_ui:** add the table view with Correspondents, thread trees and an unthreaded message list ([49aac56](https://github.com/pmig/maze/commit/49aac569f8bd7b409fedb238a2ba71c8d72695ac))
* **mail_ui:** delete mail with backspace, asking first in Trash ([ac0f660](https://github.com/pmig/maze/commit/ac0f6600e432e828ba0f20f024660de3e5ddf245))
* **mail_ui:** explain settings.json in a banner with a link to the defaults ([7c459b9](https://github.com/pmig/maze/commit/7c459b9319d27a7deccb29a2af1f5d32ecdbee2a))
* **mail_ui:** favorite folders section in the folder pane ([d7cf4c8](https://github.com/pmig/maze/commit/d7cf4c84a6243b96887203c69aab12f5fb158aff))
* **mail_ui:** list each account's special folders first, above a divider ([d577bf5](https://github.com/pmig/maze/commit/d577bf54176717fcfe7cf7937191d06054588296))
* **mail_ui:** reply with r and reply all with shift-r in the list ([74d5a62](https://github.com/pmig/maze/commit/74d5a62f0c61b1b0bfe313668c41542ae5ee098e))
* **mail_ui:** select multiple messages and quick-move them with shift-m ([4b0fb6a](https://github.com/pmig/maze/commit/4b0fb6a29dbb86e90561d1fcaa25aa759ef162d9))
* **mail_ui:** show link targets on hover, open links safely and add a sync button ([34f7577](https://github.com/pmig/maze/commit/34f7577ddd8e546bd21848ee4565470ab4668c0b))
* **mail_ui:** show the original HTML of a message with an embedded Servo engine ([f38fa26](https://github.com/pmig/maze/commit/f38fa265dfe016b511502ff1af7e6ff2d740550a))
* **mail_ui:** show the sender's mail client ([af8f93e](https://github.com/pmig/maze/commit/af8f93e443ed37d139208f9fa634228dc402d1ae))
* **mail_ui:** sort by any column and add a message context menu ([6d2bc34](https://github.com/pmig/maze/commit/6d2bc3466495adde5f8e6e7d61907f9599d28c36))
* **mail_ui:** undo the last archive, move or flag change with cmd-z ([2318e72](https://github.com/pmig/maze/commit/2318e72c9fec800bc79deec78161d24f5b6bed7f))
* **mail_ui:** use one message list view for every folder ([b19edac](https://github.com/pmig/maze/commit/b19edace3f6109867d32840e99d86f554b7d2d65))
* **mail_ui:** Zed-style panes with tabs and splits, drafts in tabs ([4e76b9a](https://github.com/pmig/maze/commit/4e76b9a5255a62e1730e72281c530d064901c7f0))
* **mazemail:** test an account's connection before adding it ([f19e5f0](https://github.com/pmig/maze/commit/f19e5f0612ffe22be8a275976bcfbe30099e3893))
* **mime:** parse, thread and sanitize messages and build reader documents ([0c567f0](https://github.com/pmig/maze/commit/0c567f043cfb45fc5e4ec7f643b1bd4391eb60d0))
* notifications for new mail ([2444c1b](https://github.com/pmig/maze/commit/2444c1b546aa6bddfbd9d76707902d29293c79b6))
* open and save attachments ([69cf008](https://github.com/pmig/maze/commit/69cf0088d649911467db5176d832a5c3362cb069))
* report junk with a button next to Archive and the j key ([0045c81](https://github.com/pmig/maze/commit/0045c81b844564a931767749d1f9eceaf4ec66bb))
* **search:** add the query language, encrypted Tantivy index, facets and snippets ([6e8530c](https://github.com/pmig/maze/commit/6e8530cf6c1bff3c3d37f6a2a077a79e9899f442))
* **security:** verify DKIM, ARC and DMARC, trust provider results and flag phishing ([b593cc2](https://github.com/pmig/maze/commit/b593cc283d3e275bb8e86fcfe08fbe069b06b48b))
* **store:** add the encrypted SQLite store, blob store, migrations and thread lists ([639a961](https://github.com/pmig/maze/commit/639a961bb903f2ded61789982a285633df6e2960))
* **sync:** add per-account IMAP sync with QRESYNC/CONDSTORE passes, IDLE, Gmail labels and op replay ([c434b3a](https://github.com/pmig/maze/commit/c434b3a6da40a80e920c051a747ba2f12283fcb8))
* **sync:** watch favorite folders and notify new mail in them ([270a801](https://github.com/pmig/maze/commit/270a80186f4f1b18d4f4d12957ad7efeb4b4bbfb))
* **types:** add mazemail_types with ids, flags, folder roles and addresses ([a024f50](https://github.com/pmig/maze/commit/a024f50745a6977dc96e86353298af1b721e11d3))
* **ui:** add the Maze window with four-column layout, themes and Thunderbird keymap ([68bb639](https://github.com/pmig/maze/commit/68bb639c098efa07e013e9cebfeb4de678839409))
* **ui:** scale the layout with a font_size setting ([d57a7df](https://github.com/pmig/maze/commit/d57a7df5d8b8d424653b71563f90a4ef5cdb6c45))
* **vault:** add master key handling, encrypted blob and block formats and recovery keys ([10b8506](https://github.com/pmig/maze/commit/10b850620355d92ed0924c5da87189923e1c0237))
* **website:** add the mazemail.dev Astro site with Cloudflare static hosting ([335e4cb](https://github.com/pmig/maze/commit/335e4cb83de8c73b18170873a33e9dff49aa8c51))
* **website:** call Maze the agentic email client ([1ab47ba](https://github.com/pmig/maze/commit/1ab47ba61ef24ad339ea6f9137515b733f13062e))
* **website:** new Maze logo, an envelope with a terraced flap ([f031b82](https://github.com/pmig/maze/commit/f031b823781152e121863f1ab2e8aa1a0430ae0a))
* **website:** new tagline, drop the status and licence badges ([3a42ee2](https://github.com/pmig/maze/commit/3a42ee2a88c11a35408b4ed2a85a30500fb2aee4))
* **website:** show light or dark screenshots to match the theme ([0f671cc](https://github.com/pmig/maze/commit/0f671ccbe2812294eda874a3c7bb6f7f1321f574))
* **website:** show the screenshot once ([36f5898](https://github.com/pmig/maze/commit/36f5898f440b96f992f9580daaa33a8b1a74b686))
* wire the Maze window, mazemail CLI and headless socket server to the encrypted core ([8c0eb6d](https://github.com/pmig/maze/commit/8c0eb6d0eec107387174a339d66eb77bdf145034))


### Bug Fixes

* **app:** open the Add Account dialog and update notices after the triggering event ([0475eba](https://github.com/pmig/maze/commit/0475eba0dba0a73ed48708078de34d31527da8a8))
* **deps:** bump ctutils to 0.4.5 for RUSTSEC-2026-0385 ([74566c8](https://github.com/pmig/maze/commit/74566c8fb42f613ac1ee4369a4a470a4b8e0cd0f))
* **imap:** read Gmail's quoted system labels, re-read labels once ([0ef821b](https://github.com/pmig/maze/commit/0ef821bcc0284a513cf6ae7932fa099405e0b3c2))
* **mail_ui:** open settings.json from the settings button in the spaces bar ([2413476](https://github.com/pmig/maze/commit/2413476d93c22662113450d6b46bab36241b8046))
* **mail_ui:** show list shortcuts like archive in the command palette ([d76d15e](https://github.com/pmig/maze/commit/d76d15ef82a6160dd3904412b7780ba0fbfc2527))
* **mail_ui:** show the icon and the sort arrow in the star, unread and attachment columns ([ec62428](https://github.com/pmig/maze/commit/ec62428c8a8bf264151b48a1356d3dbea61505dc))
* **mail_ui:** stop at the first and last row of the table ([d32636b](https://github.com/pmig/maze/commit/d32636bdd66ab9a0077be62b5c2bc144c3cd4f70))
* point every GitHub link at pmig/maze ([63282ff](https://github.com/pmig/maze/commit/63282ff60b3d3212ce69ec0730c85f6a17efb672))
* **security:** trust the receiving server's SPF result for any IMAP account ([cd5b855](https://github.com/pmig/maze/commit/cd5b85589f1092a2812bd27cda03466b51b8cc89))
* **settings:** report diagnostics in a stable order ([a561e03](https://github.com/pmig/maze/commit/a561e03a500ac52b9410867617861627f3068978))
* **ui:** embed the star-plus icon ([b1ce26d](https://github.com/pmig/maze/commit/b1ce26db0057924410c9bffc465b79bf1a7d248f))
* **ui:** give code editors the app theme's editor and syntax colors ([c08d4cc](https://github.com/pmig/maze/commit/c08d4ccb82a918d43806b61be1bd883d74eb77be))
* use aws-lc-rs as the only rustls provider, so the original view loads ([8ea2f99](https://github.com/pmig/maze/commit/8ea2f99cae37b45f0618ff85bff3e9dd291d91d2))


### Performance

* **store:** batch blob durability per transaction and pin the Message-ID index for thread lookups ([bb1abf7](https://github.com/pmig/maze/commit/bb1abf74ae14b2c10dd7dc3cf6dcd09438694dc9))


### Refactoring

* IMAP only — drop Local Folders and POP, import mbox into IMAP folders ([f609d43](https://github.com/pmig/maze/commit/f609d438d0d2eb6af829af175c5ec061620c202c))


### Docs

* add ADR-001 to ADR-015, contributing guide and security policy ([a91c14a](https://github.com/pmig/maze/commit/a91c14a119a4800d50676970ddf26800ea3385f2))
* add the app icon, user guides, release checklist and a rewritten README ([9527be4](https://github.com/pmig/maze/commit/9527be498db3d6d13b8ad92dc2b0487f8049d796))
* **adr:** add ADR-016 for Cursor and pi as ACP agents ([68d6bff](https://github.com/pmig/maze/commit/68d6bffa4995df18c181346b35f797aea1e5013c))
* bring the website and docs up to date with shipped features ([b02b478](https://github.com/pmig/maze/commit/b02b4789c29b7d6151b1e778171cd9faf6bee99a))
* build the website with pnpm in the docs rule ([5644f8d](https://github.com/pmig/maze/commit/5644f8da4fd7a2fc5530a0c19cdc0b20f2f9d347))
* escape placeholders that Markdown renders as HTML ([641b003](https://github.com/pmig/maze/commit/641b00310883a39f98e275ebd9c8678c7ec6b6af))
* explain Gmail app passwords and the Microsoft client ID ([50bd1b0](https://github.com/pmig/maze/commit/50bd1b0008535206ab215011597972c5b5961036))
* new subtitle in the README and on the social card ([4807411](https://github.com/pmig/maze/commit/4807411d29822aba67ba645fbd09963af7fb520d))
* rename core and mime crates to avoid name collisions ([eb5a33a](https://github.com/pmig/maze/commit/eb5a33a0d4d0798302af5f6f31946a859460c1e0))
* require docs and website updates with every feature ([56eb032](https://github.com/pmig/maze/commit/56eb032af56e43c5e099923309bb64c5ece00a6d))


### Other

* **deps:** update libz-sys to 1.1.31 (RUSTSEC-2026-0418) ([de3c601](https://github.com/pmig/maze/commit/de3c601ae79376dec1a2b214765b8827dc87365c))
* give rust-analyzer its own target dir in Zed ([11e97ef](https://github.com/pmig/maze/commit/11e97ef685a7b4e20819b73eed30373cc094d94c))
* pin the first release to 0.1.0-alpha.1 ([b3dc4b1](https://github.com/pmig/maze/commit/b3dc4b1c76e9176594762398a380132abe9b247a))
* scaffold cargo workspace, toolchain and repository conventions ([d00f113](https://github.com/pmig/maze/commit/d00f113375a72ff9962bb877b6b2842a3e5bdae1))
* silence dead_code warnings in the vendored zed-font-kit ([7260b48](https://github.com/pmig/maze/commit/7260b488cbb943818ccea6384e3bc40d85bb4c1a))
