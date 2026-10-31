# Changelog

## 0.1.0-alpha.1 (2026-10-31)


### ⚠ BREAKING CHANGES

* Maze no longer has Local Folders or local accounts, and mbox/Maildir imports no longer go into a local account. Updating removes empty Local Folders; Local Folders that still hold mail stay readable, searchable and exportable (never synced) until a later version removes them. Keep that mail with `mazemail export 'account:"Local Folders"' --to local-folders.mbox` and `mazemail import local-folders.mbox --account <account>`. `mazemail import` of an mbox file or Maildir now needs an IMAP account (`--account` unless there is only one) and uploads to the server.

### Features

* add IMAP, Gmail and Microsoft 365 accounts with live sync in the app and CLI ([1cc90bc](https://github.com/pmig/maze/commit/1cc90bcb34442f2f3f8d567617e95cdfc2165748))
* add mazemail binary skeleton with CLI entry point ([a5719b8](https://github.com/pmig/maze/commit/a5719b8de92afe318f8fd09d5d158b2c48dfb7ca))
* add release channels, settings, auto-updater core and release manifest xtask ([587b408](https://github.com/pmig/maze/commit/587b4089f2e647b429a67b6cf971962a009c160a))
* add the in-app auto-updater with Restart to update, rollback and update notices ([d32096f](https://github.com/pmig/maze/commit/d32096f649f4eecdee7f5c1daac2db052ef6e0ad))
* **agent_ui:** sign in to agents, pick models, auto-update, Zed-style panel ([aa74e64](https://github.com/pmig/maze/commit/aa74e64dda6c4cbbed1392457036662af4521ce1))
* **agent:** run ACP agents sandboxed in the agent column ([113b8f1](https://github.com/pmig/maze/commit/113b8f13a2acfd82dd9682dce85fa001cf02fa1e))
* apply the theme setting, with Zed themes from the themes folder ([2f1d940](https://github.com/pmig/maze/commit/2f1d9404b42e3dcbe8b5d67f9141ad2e23462adb))
* **auth:** add OAuth2 PKCE sign-in for Google and Microsoft, token refresh and keychain secrets ([ab299da](https://github.com/pmig/maze/commit/ab299da55f1eb8723bd0e9e9772c8676b7fc323d))
* **auth:** keep account secrets in the encrypted store, one keychain item ([2b41724](https://github.com/pmig/maze/commit/2b417248d1f0eb9f20be10d5f85ab5350dfd9d46))
* **auth:** per-resource tokens and contacts consent ([1e27c17](https://github.com/pmig/maze/commit/1e27c17438ebd0c5c262d5b823ca307827f34def))
* **composer:** check spelling as you type, with suggestions and a personal dictionary ([3a6e7ef](https://github.com/pmig/maze/commit/3a6e7ef4550bccec148e68a662a30538dd72c6a6))
* **composer:** make "Sent with Maze" a setting only ([ddb6361](https://github.com/pmig/maze/commit/ddb636121cf23ae23d333af59fe6e99bb735b511))
* **composer:** recipient chips with autocomplete and drag and drop ([65a2560](https://github.com/pmig/maze/commit/65a25603bbbc4c75d67be1bac9b4508b3e64626f))
* **composer:** write and send mail ([83a8514](https://github.com/pmig/maze/commit/83a8514c14d5a63aed4221024a6ebffdfe2e628d))
* **contacts:** sync Google contacts with write-through editing ([4d2dd4a](https://github.com/pmig/maze/commit/4d2dd4a2d79a9b7b5f9d260ec30cbc532606a84e))
* **core:** add the CoreHandle facade with queries, search, commands, undo and events ([44cc53c](https://github.com/pmig/maze/commit/44cc53c9f30fddf47c8aa6da8f98bb97e68aa532))
* disk usage and sync activity in the status bar, Microsoft tenant ([2547801](https://github.com/pmig/maze/commit/2547801b83f35001771db035d31b4d1a1637e45d))
* edit settings.json in a tab, compose footer and "Sent with Maze" ([0e1c9d2](https://github.com/pmig/maze/commit/0e1c9d24165d993368dc751c68edaab4770d5b82))
* go to folder on cmd-k, search on cmd-shift-f, Ctrl as cmd on Linux ([80e496b](https://github.com/pmig/maze/commit/80e496bcb554cc456ea50497d44ddbd04cf59f8f))
* **imap:** add the IMAP protocol layer with CONDSTORE, QRESYNC, UIDPLUS, Gmail and a fake server ([57e7c61](https://github.com/pmig/maze/commit/57e7c61e83f732e9199a242bfb47d621bec79653))
* **import:** add the shared ingest pipeline and mbox, Maildir and Thunderbird profile import ([eb65267](https://github.com/pmig/maze/commit/eb65267afa808256ced65075a5aabbc9b31affb3))
* **local_api:** add the socket protocol, command registry, scoped tokens and approval flow ([fa01bdd](https://github.com/pmig/maze/commit/fa01bdd385c3e332b48e810dd131282e4ba1da68))
* **mail_ui:** add the table view with Correspondents, thread trees and an unthreaded message list ([99250a8](https://github.com/pmig/maze/commit/99250a8ed7d1b3fe74e4e37da70a0c5d90ba6397))
* **mail_ui:** an unread dot column in the table, click to toggle ([3e3981b](https://github.com/pmig/maze/commit/3e3981b78a03a8870daae86103646a803724d267))
* **mail_ui:** delete mail with backspace, asking first in Trash ([0ba3aea](https://github.com/pmig/maze/commit/0ba3aea700127758c13b32408b5aaef348927284))
* **mail_ui:** explain settings.json in a banner with a link to the defaults ([4aa1c96](https://github.com/pmig/maze/commit/4aa1c9634df00ef57ba8a40fce667eb1b1ed45f5))
* **mail_ui:** favorite folders section in the folder pane ([10c1656](https://github.com/pmig/maze/commit/10c165676681c30bdfd62b3b6a1d409ba2c8aaa9))
* **mail_ui:** list each account's special folders first, above a divider ([cbd08d5](https://github.com/pmig/maze/commit/cbd08d5cab77a2d46aa6f680ca581092a47bd18d))
* **mail_ui:** move through the list with up and down from the message pane ([9c66278](https://github.com/pmig/maze/commit/9c66278acdeed2ebc1a86c5a2e631688b8c2cd4e))
* **mail_ui:** reply with r and reply all with shift-r in the list ([5bffff9](https://github.com/pmig/maze/commit/5bffff9373d62cdf305e9ab00132e9145d2b7b74))
* **mail_ui:** select multiple messages and quick-move them with shift-m ([669d68b](https://github.com/pmig/maze/commit/669d68bc7a88021f62235e6b5ed80692beda88c7))
* **mail_ui:** show link targets on hover, open links safely and add a sync button ([be7de5d](https://github.com/pmig/maze/commit/be7de5db674bcdab7749e53ffdf33c5fd5a51978))
* **mail_ui:** show the original HTML of a message with an embedded Servo engine ([fc9b4e4](https://github.com/pmig/maze/commit/fc9b4e4f22741b974f979b4c8068270db3ed887a))
* **mail_ui:** show the sender's mail client ([d6e939e](https://github.com/pmig/maze/commit/d6e939e47ebf3816a27c8024518e332b74dfad0b))
* **mail_ui:** sort by any column and add a message context menu ([595bb15](https://github.com/pmig/maze/commit/595bb15651ba22b336cafbb984d81be07036a32e))
* **mail_ui:** the Address Book space with all accounts in one list ([bbefaf8](https://github.com/pmig/maze/commit/bbefaf8276da3339979b87e39c93878082ff918b))
* **mail_ui:** undo the last archive, move or flag change with cmd-z ([e66d6b2](https://github.com/pmig/maze/commit/e66d6b20c386c261be66c6148782070554e59908))
* **mail_ui:** use one message list view for every folder ([6329659](https://github.com/pmig/maze/commit/6329659b93a38ec840c6733b21fea31b656f2c7c))
* **mail_ui:** Zed-style panes with tabs and splits, drafts in tabs ([2588fe7](https://github.com/pmig/maze/commit/2588fe795e190cbdde1d5255aa8f68f00725cf56))
* **mazemail:** test an account's connection before adding it ([a3c6b54](https://github.com/pmig/maze/commit/a3c6b54f5965ff5caa233ac3c0cd5320b80bb094))
* **mime:** parse, thread and sanitize messages and build reader documents ([14376e4](https://github.com/pmig/maze/commit/14376e4e252d45dbb3dad3d5e1dd27a58efcd050))
* notifications for new mail ([15e6647](https://github.com/pmig/maze/commit/15e6647100dd8922c55e9f8368511116168c412a))
* open and save attachments ([6308c4b](https://github.com/pmig/maze/commit/6308c4bb9ab2efbbad7350b2fd06196fdc32da47))
* report junk with a button next to Archive and the j key ([9c1a57c](https://github.com/pmig/maze/commit/9c1a57c5c5732fe6097310c3e75721d3773c2b7c))
* **search:** add the query language, encrypted Tantivy index, facets and snippets ([30c08cb](https://github.com/pmig/maze/commit/30c08cb0857b633794559613957fb209dff3be8d))
* **security:** verify DKIM, ARC and DMARC, trust provider results and flag phishing ([601162e](https://github.com/pmig/maze/commit/601162ebc0ac027b720e2f129cbfafa54b73e98b))
* **spelling:** add the spell checking engine with bundled dictionaries ([3626453](https://github.com/pmig/maze/commit/362645343d5e32f44af7a83e85dc60fed822e4f8))
* **store:** add the encrypted SQLite store, blob store, migrations and thread lists ([5294c60](https://github.com/pmig/maze/commit/5294c60fb99c3d8abcdf8af7bb312eb025ae2680))
* **sync:** add per-account IMAP sync with QRESYNC/CONDSTORE passes, IDLE, Gmail labels and op replay ([a2c4c57](https://github.com/pmig/maze/commit/a2c4c57f8bec5d58bd71f026d1f68eb8e13d8f43))
* **sync:** watch favorite folders and notify new mail in them ([a71f4d6](https://github.com/pmig/maze/commit/a71f4d662bbadbc9738bc8d80c1f267d3be9d81e))
* **types:** add mazemail_types with ids, flags, folder roles and addresses ([8d58eff](https://github.com/pmig/maze/commit/8d58effb67444a7b9accaedf36f0a746cbd71283))
* **ui:** add the Maze window with four-column layout, themes and Thunderbird keymap ([cdcb5db](https://github.com/pmig/maze/commit/cdcb5dbf31a093d3d6d3da5713451df44f446762))
* **ui:** scale the layout with a font_size setting ([41961b2](https://github.com/pmig/maze/commit/41961b2cc7d971705bd523f49e2f78863437120a))
* **vault:** add master key handling, encrypted blob and block formats and recovery keys ([20cfee4](https://github.com/pmig/maze/commit/20cfee4aeed4dc8877c42f94a5967d44bf5e8001))
* **website:** add the mazemail.dev Astro site with Cloudflare static hosting ([d7bdc82](https://github.com/pmig/maze/commit/d7bdc821fb3d748ee48821b14a094c9db19c6f23))
* **website:** call Maze the agentic email client ([f57ee7e](https://github.com/pmig/maze/commit/f57ee7e51be01cba7c8834a05205b3157fa7807b))
* **website:** new Maze logo, an envelope with a terraced flap ([27c06bd](https://github.com/pmig/maze/commit/27c06bd6a16ed112187f87042eb53be43255de84))
* **website:** new tagline, drop the status and licence badges ([a567a7d](https://github.com/pmig/maze/commit/a567a7d9875a59ca1924ff622dae4240dae89d09))
* **website:** show light or dark screenshots to match the theme ([5f79193](https://github.com/pmig/maze/commit/5f79193c29e1a95c8fab35503248f0131974874d))
* **website:** show the screenshot once ([078f9af](https://github.com/pmig/maze/commit/078f9af8e0e7ff966bfa54c4e92016082f644cec))
* wire the Maze window, mazemail CLI and headless socket server to the encrypted core ([8ccc523](https://github.com/pmig/maze/commit/8ccc52302d848ee5608e7482da03d39756d1509a))


### Bug Fixes

* **app:** open the Add Account dialog and update notices after the triggering event ([202ac5f](https://github.com/pmig/maze/commit/202ac5f8bcf981f65405cc5212733221dca76e32))
* **build:** make the started Maze Dev.app the only one macOS knows ([25830cc](https://github.com/pmig/maze/commit/25830cc850f73f1a2143559a22522d863a47373d))
* **deps:** bump ctutils to 0.4.5 for RUSTSEC-2026-0385 ([0b7571c](https://github.com/pmig/maze/commit/0b7571cb97546cd485065de2ebcbe52f37c41e9c))
* **imap:** read Gmail's quoted system labels, re-read labels once ([e7ba0ed](https://github.com/pmig/maze/commit/e7ba0eda9c87051efe3b943edd70bde1ab6058c8))
* **mail_ui:** open settings.json from the settings button in the spaces bar ([a2f7098](https://github.com/pmig/maze/commit/a2f70984f1489281c6db167d11584a16745e85c2))
* **mail_ui:** show list shortcuts like archive in the command palette ([09eca91](https://github.com/pmig/maze/commit/09eca910a650ca659c870e09953def54acf0bdc8))
* **mail_ui:** show the icon and the sort arrow in the star, unread and attachment columns ([6f29cde](https://github.com/pmig/maze/commit/6f29cde889f9ebb605a03c869cb5f1bcd16eb115))
* **mail_ui:** stop at the first and last row of the table ([2b0ac5a](https://github.com/pmig/maze/commit/2b0ac5a4ee2704135ebcbd0153a98ce3dcd6a0ee))
* point every GitHub link at pmig/maze ([1dd08f5](https://github.com/pmig/maze/commit/1dd08f5f61616d45626f5a2961fde22264115692))
* regenerate Maze.icns with the new app icon ([7a9f0e3](https://github.com/pmig/maze/commit/7a9f0e33d90dabbb92595d697879ec55429cb522)), closes [#2](https://github.com/pmig/maze/issues/2)
* **security:** trust the receiving server's SPF result for any IMAP account ([9857312](https://github.com/pmig/maze/commit/985731281f47a6b54aabfb470d7e17bfe21a289f))
* **settings:** report diagnostics in a stable order ([6bf80e6](https://github.com/pmig/maze/commit/6bf80e6fdc132948ba02c27eb0b5b1ea1e8845fa))
* **store:** stop and join the writer when the last store clone is dropped ([2452d2c](https://github.com/pmig/maze/commit/2452d2c097d4d360d3f804a089dee3fa88c5ad3e)), closes [#3](https://github.com/pmig/maze/issues/3)
* **sync:** judge the offline policy's "recent" by the message date in every path ([dbb6ee9](https://github.com/pmig/maze/commit/dbb6ee95424b14a467a8002b3d982f6b786826e0)), closes [#5](https://github.com/pmig/maze/issues/5)
* **sync:** publish the folder activity before the status ([bf7ab98](https://github.com/pmig/maze/commit/bf7ab98ffe25e16f92d535d5f55b7cfe48cc15c0))
* **ui:** embed the star-plus icon ([1372ae6](https://github.com/pmig/maze/commit/1372ae64dc20e315cd9efb71018bed3876d4e02f))
* **ui:** give code editors the app theme's editor and syntax colors ([8a742d1](https://github.com/pmig/maze/commit/8a742d18d59010befff0d1d35452635ce6e65365))
* use aws-lc-rs as the only rustls provider, so the original view loads ([3c2debc](https://github.com/pmig/maze/commit/3c2debc600dd2c532ed325dbcb06229e8fc39ad6))


### Performance

* **store:** batch blob durability per transaction and pin the Message-ID index for thread lookups ([89c827a](https://github.com/pmig/maze/commit/89c827ab3ffdcd62ed3a3a5be1e6459f206600e7))


### Refactoring

* **composer:** write the body in gpui-kit's editor set up for prose ([2cef931](https://github.com/pmig/maze/commit/2cef931d0b637030b4f84af42d692052d7797fef))
* IMAP only — drop Local Folders and POP, import mbox into IMAP folders ([4c1210f](https://github.com/pmig/maze/commit/4c1210f0de59f9fb66c1f420f304449d55d17b2b))


### Docs

* add ADR-001 to ADR-015, contributing guide and security policy ([382e4a5](https://github.com/pmig/maze/commit/382e4a526f52f8730b4aa2897c0454b749e581f2))
* add the app icon, user guides, release checklist and a rewritten README ([0dd80f1](https://github.com/pmig/maze/commit/0dd80f18b52fd6c9411dd1f90e34502317a3ab60))
* **adr:** add ADR-016 for Cursor and pi as ACP agents ([0260b02](https://github.com/pmig/maze/commit/0260b02fecf560c331b84f4774ba71c516c48ed9))
* bring the website and docs up to date with shipped features ([744d57a](https://github.com/pmig/maze/commit/744d57a20e31cb3ce76dca7c1a9f48b4b6db8f8c))
* build the website with pnpm in the docs rule ([d75ed71](https://github.com/pmig/maze/commit/d75ed710fb4b6710959f1c91b1baa9fc2f4d6e8a))
* escape placeholders that Markdown renders as HTML ([f850524](https://github.com/pmig/maze/commit/f85052452c8f98e16143ba9f66a658f9e2fb0b78))
* explain Gmail app passwords and the Microsoft client ID ([0d4234e](https://github.com/pmig/maze/commit/0d4234e889f2960477b449e798ad8441421801e9))
* new subtitle in the README and on the social card ([5f4c4e4](https://github.com/pmig/maze/commit/5f4c4e47515010879eec1407e29c75197aad1e29))
* rename core and mime crates to avoid name collisions ([16a2891](https://github.com/pmig/maze/commit/16a2891f7649d49dc3426d962c16656336911248))
* require docs and website updates with every feature ([1c6db34](https://github.com/pmig/maze/commit/1c6db349ecb22d0c8ce3f086f986d91a75e8e68f))


### Other

* **deps:** update libz-sys to 1.1.31 (RUSTSEC-2026-0418) ([2f74331](https://github.com/pmig/maze/commit/2f74331609990674ca9ee4725f278128c3a8204e))
* give rust-analyzer its own target dir in Zed ([97bda60](https://github.com/pmig/maze/commit/97bda60cc2883ee5e572676ed108ec684b506f54))
* pin the first release to 0.1.0-alpha.1 ([b3e54e8](https://github.com/pmig/maze/commit/b3e54e808d5e71c0a1ca9ccdb7b1834df8fe92f5))
* scaffold cargo workspace, toolchain and repository conventions ([c978589](https://github.com/pmig/maze/commit/c97858915ad001920bbdfaefe086b37a823cbc02))
* silence dead_code warnings in the vendored zed-font-kit ([b93ebd0](https://github.com/pmig/maze/commit/b93ebd0437b193055e597275660174059f4507ac))
