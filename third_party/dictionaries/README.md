# Spelling dictionaries

Hunspell dictionaries for the `spelling` crate (`crates/spelling`), which embeds them in
the Maze binary and decompresses each language on first use. They are data, not code.

## Source

All five come from [wooorm/dictionaries](https://github.com/wooorm/dictionaries) at
commit [`8cfea406b505e4d7df52d5a19bce525df98c54ab`](https://github.com/wooorm/dictionaries/tree/8cfea406b505e4d7df52d5a19bce525df98c54ab/dictionaries)
(2024-09-09; the repository has no tags), downloaded on 2026-10-30. wooorm/dictionaries
normalizes upstream Hunspell dictionaries to UTF-8; the files are unchanged except for
the zstd compression.

| Here | Upstream directory | Upstream source | License |
|---|---|---|---|
| `en-US/` | `dictionaries/en` (dictionary-en 4.0.0) | [SCOWL](http://wordlist.aspell.net) 2020.12.07, en_US, size 60 | MIT AND BSD (see `LICENSE`) |
| `en-GB/` | `dictionaries/en-GB` (dictionary-en-gb 3.0.0) | SCOWL 2020.12.07, en_GB-ise, size 60 | MIT AND BSD (see `LICENSE`) |
| `de-DE/` | `dictionaries/de` (dictionary-de 3.0.0) | [igerman98](https://www.j3e.de/ispell/igerman98/) 20161207, de_DE | GPL-2.0 OR GPL-3.0 |
| `de-AT/` | `dictionaries/de-AT` (dictionary-de-at 3.0.0) | igerman98 20161207, de_AT | GPL-2.0 OR GPL-3.0 |
| `de-CH/` | `dictionaries/de-CH` (dictionary-de-ch 3.0.0) | igerman98 20161207, de_CH | GPL-2.0 OR GPL-3.0 |

Each directory holds `index.aff.zst` and `index.dic.zst` (upstream `index.aff` and
`index.dic`, zstd level 19) and `LICENSE` (upstream `license`, verbatim).

| Language | `.aff` raw → zstd | `.dic` raw → zstd |
|---|---|---|
| en-US | 3,086 → 908 | 551,762 → 169,700 |
| en-GB | 3,086 → 908 | 552,106 → 169,634 |
| de-DE | 19,199 → 3,957 | 1,118,194 → 282,225 |
| de-AT | 19,199 → 3,958 | 1,121,822 → 283,191 |
| de-CH | 19,199 → 3,958 | 1,119,710 → 282,649 |

Total: 4.53 MB raw, 1.20 MB compressed (what the binary grows by).

SHA-256 of the uncompressed upstream files:

```
f0b1a234bd178bdd01875b2a392a9647f888b8fe879f79c52aae62c2759b3647  en/index.dic
8ae1f19d4840d957728ad90555d5a8dff6cc5c046279c95ff0c00fc0a0136c7b  en/index.aff
869fe17ba4ee4b5401c60a666ee2d6a3dcc237f460b7294435df1cc6a799aa57  en-GB/index.dic
8ae1f19d4840d957728ad90555d5a8dff6cc5c046279c95ff0c00fc0a0136c7b  en-GB/index.aff
b5c781a0cf6f285fb6b9b8ab02fbea104b987104a1efdda8a835837e89e3ec77  de/index.dic
57fdd1b16aac2131003c91e0cf2a488becb970382a402a9ce089307301cb3ef0  de/index.aff
02cbbeb74af85497b861b5782413b6c8c8ffab5c9423ef2e92265599b9359870  de-AT/index.dic
04ebc966a0b1130d8fc62886bb801e77bf7d6a11accf205954c364d3c0c68864  de-AT/index.aff
abb1abcc1951abba255d4e6289e5d44293338ae29d1331dea16c75bd89a43e1c  de-CH/index.dic
11d773a903ece02c71b5094d71dcf3afe27cad0383cfe063fddbde26b8443785  de-CH/index.aff
```

## Licenses

- **English (SCOWL):** the word lists are Copyright 2000-2018 Kevin Atkinson and others
  under permissive MIT-like terms, the affix file derives from Geoff Kuenning's Ispell
  (BSD); the full notices are in `en-US/LICENSE` and `en-GB/LICENSE` and must ship with
  the dictionaries.
- **German (igerman98):** Copyright (C) 1999-2016 Björn Jacke, licensed under the GNU
  GPL **version 2 or version 3** (not "or any later version"). Maze is
  GPL-3.0-or-later; Maze distributes these dictionaries under **GPL-3.0**. The affix
  files ask for copies of both licenses with every distribution: `GPL-2.0.txt` and
  `GPL-3.0.txt` here. Should Maze ever move to a GPL version after 3, these dictionaries
  have to be replaced or relicensed.

## Updating

Download into a fresh directory (the files are untrusted data; nothing in them is run),
review the diff, then compress with the zstd CLI (decoded at runtime by the pure-Rust
`ruzstd`):

```sh
commit=<new commit>
for pair in en:en-US en-GB:en-GB de:de-DE de-AT:de-AT de-CH:de-CH; do
  src=${pair%%:*} dst=${pair##*:}
  for f in index.aff index.dic license; do
    curl -fsSL -o "$tmp/$src-$f" \
      "https://raw.githubusercontent.com/wooorm/dictionaries/$commit/dictionaries/$src/$f"
  done
  zstd -19 -f "$tmp/$src-index.aff" -o "$dst/index.aff.zst"
  zstd -19 -f "$tmp/$src-index.dic" -o "$dst/index.dic.zst"
  cp "$tmp/$src-license" "$dst/LICENSE"
done
```

`zstd` without a system install: `mise x conda:zstd@1.5.7 -- zstd …`. Then update the
commit, sizes and checksums above and run `cargo nextest run -p spelling`.
