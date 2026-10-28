# mazemail.dev

The website for Maze: a landing page, downloads, the docs, the roadmap and the
changelog. It's a fully static [Astro](https://astro.build) site served by
[Cloudflare Workers static assets](https://developers.cloudflare.com/workers/static-assets/).
The design rules are in [DESIGN.md](DESIGN.md).

- **No duplication.** `/docs/*` renders `../docs/*.md` in place (an Astro content
  collection with a glob loader), `/changelog/` renders `../CHANGELOG.md` once
  release-please writes it, and the favicon comes from `../assets/icons/maze.svg`. Edit
  those files, not copies.
- **No client framework, no third parties.** Two scripts: `public/theme.js` (theme
  toggle, the `d` shortcut and naming the visitor's computer, about 4 KB) and `public/dots.js` (the hero's dot
  animation, home page only, about 15 KB). Fonts (IBM Plex Sans, Lilex; both
  SIL OFL 1.1) are configured in `astro.config.mjs` (`fonts`, npm provider) and
  self-hosted under `/_astro/fonts/`. The CSP in `public/_headers` allows nothing but
  the site's own origin.
- **Plain CSS** with custom properties (`src/styles/global.css` plus scoped `<style>`
  in components). A site this size doesn't need Tailwind's build step or class soup.

## Layout

```
website/
├── astro.config.mjs        site URL, static output, fonts, CSP hashes, Markdown, sitemap
├── wrangler.jsonc          Cloudflare: assets-only Worker, custom domain
├── public/
│   ├── _headers            CSP, HSTS, Permissions-Policy, cache rules
│   ├── _redirects          a few convenience redirects
│   ├── .well-known/security.txt
│   ├── og.png              social card (made from scripts/og.html)
│   ├── dots.js             hero dot animation (home page)
│   └── theme.js
├── scripts/
│   ├── check-links.mjs     internal link + anchor checker for dist/
│   └── og.html             source of public/og.png
└── src/
    ├── content.config.ts   the docs collection (../docs)
    ├── data/               site facts, numbers, docs navigation
    ├── lib/                release lookup, Markdown plugins, icon
    ├── layouts/            Base, Docs
    ├── components/
    ├── pages/              /, /download/, /docs/, /roadmap/, /changelog/, 404
    └── styles/global.css
```

## Develop

Node is pinned in `.node-version` and pnpm in `package.json` (`packageManager`);
with [mise](https://mise.jdx.dev), `mise install` in this folder sets up both (plus
the repository's Rust tools from the root `mise.toml`, which mise always merges in).

```sh
cd website
pnpm install
pnpm dev            # http://localhost:4321, reloads on changes (also: pnpm start)
pnpm check          # astro check: TypeScript (strictest) and template diagnostics
pnpm build          # → dist/
pnpm check:links    # every internal link and #anchor in dist/ resolves
pnpm preview        # serve dist/ locally
```

`pnpm exec wrangler dev` serves `dist/` the way Cloudflare will (with `_headers`,
`_redirects`, the 404 page and trailing-slash redirects), without deploying anything.

At build time `src/lib/release.ts` asks the GitHub API for the newest release to link
the `.dmg` files directly. Without network access, without a release, or while the
repository is private (set `GITHUB_TOKEN` to see it), all download links fall back to
the GitHub Releases page. `MAZE_SITE_OFFLINE=1` skips the lookup.

The npm font provider reads the `@fontsource-variable/*` stylesheets from
`node_modules` but downloads the font files from jsDelivr, at the installed version, on
the first build; after that they're cached in `node_modules/.astro/fonts/`.

The social card `public/og.png` is a screenshot of `scripts/og.html`; the command to
regenerate it is at the top of that file.

## Deploy

The site is deployed by `.github/workflows/website.yaml`:

| Event                                           | What runs                                   |
| ----------------------------------------------- | ------------------------------------------- |
| Pull request touching `website/**` or `docs/**` | install, `astro check`, build, link check   |
| Push to `main` touching those paths             | the same, then `wrangler deploy`            |
| `Release artifacts` finished successfully       | rebuild and deploy (new download links)     |
| Manual (`workflow_dispatch` on `main`)          | rebuild and deploy                          |

To deploy from your machine instead (in this directory, after `mise run install` and
`wrangler login`; `mise run deploy` does the same as the command below):

```sh
pnpm run deploy     # astro build && wrangler deploy
```

### One-time Cloudflare setup

Cloudflare recommends Workers with static assets (rather than Pages) for new static
sites; that's what `wrangler.jsonc` describes: an assets-only Worker named
`mazemail-website`, no `*.workers.dev` URL, no preview URLs, served on `mazemail.dev`.

1. **Account and zone.** In a Cloudflare account, *Add a domain* → `mazemail.dev`
   (Free plan is enough) and switch the domain's nameservers at the registrar to the
   two Cloudflare gives you. Workers custom domains need the zone on Cloudflare.
2. **DNS before the first deploy.**
   - Make sure there is **no** record for `mazemail.dev` (the apex) itself; the first
     deploy creates it, along with its certificate (`routes` → `custom_domain` in
     `wrangler.jsonc`).
   - Keep `releases.mazemail.dev` → `CNAME pmig.github.io`, **DNS only** (grey cloud),
     so GitHub Pages can issue its certificate (see below).
   - For `www`: add `AAAA www 100::`, **Proxied**, then *Rules → Redirect Rules →
     Create rule from template "Redirect from WWW to root"* (301, preserve path and
     query string).
3. **Zone settings that would break the site's promises or its CSP.** Turn these off
   (they inject scripts or third-party requests into pages): *Scrape Shield → Email
   Address Obfuscation*, *Speed → Rocket Loader*, *Web Analytics / Real User
   Monitoring* for this hostname, and *Security → Bot Fight Mode* JavaScript
   detections. Recommended on: *SSL/TLS → Edge Certificates → Always Use HTTPS*,
   minimum TLS 1.2.
4. **API token.** *My Profile → API Tokens → Create Token → template "Edit Cloudflare
   Workers"*, then narrow it: *Account resources* = your account, *Zone resources* =
   `mazemail.dev` only, and a TTL if you like. The permissions the deploy actually needs
   are **Account · Workers Scripts · Edit** and **Zone · Workers Routes · Edit** (for
   the custom domain); the template adds a few read permissions wrangler uses for
   account lookups. Copy the token, and the **Account ID** from the account's overview
   page.
5. **GitHub.** *Settings → Environments → New environment* `website` (optionally
   with required reviewers or a `main`-only branch rule), and in it the secrets
   `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID`:

   ```sh
   gh secret set CLOUDFLARE_API_TOKEN --env website
   gh secret set CLOUDFLARE_ACCOUNT_ID --env website
   ```

6. **First deploy.** Merge to `main`, or run the workflow by hand (*Actions → Website →
   Run workflow*), or `pnpm run deploy` locally. Then check
   `curl -sI https://mazemail.dev/ | grep -i content-security-policy` and
   `curl -sI https://www.mazemail.dev/docs/` (a 301 to the apex).
7. **Optional, later:** once the site has run on HTTPS for a while, add `; preload` to
   the `Strict-Transport-Security` line in `public/_headers` and submit the domain at
   [hstspreload.org](https://hstspreload.org). That commits every subdomain to HTTPS.

### mazemail.dev and releases.mazemail.dev

The update manifests the app polls (`https://releases.mazemail.dev/<channel>/<os>-<arch>.json`)
are not part of this site. They're published by `release-artifacts.yaml` to the
`gh-pages` branch and served by **GitHub Pages** on `releases.mazemail.dev`
([docs/releasing.md](../docs/releasing.md)). Both live in the same Cloudflare zone and
don't interact: the Worker only answers on the apex (exact-hostname custom domain),
and `releases` is a DNS-only CNAME to GitHub. Keep that record unproxied, and don't add
a Worker route like `*.mazemail.dev/*`.

If you'd rather serve the manifests from Cloudflare later, they can move without an app
change, since the URL stays the same: publish them to an R2 bucket (or a second
assets-only Worker) attached to `releases.mazemail.dev` as a custom domain, give the
`.json` files `Cache-Control: no-cache`, switch `publish-manifests` in
`release-artifacts.yaml` from pushing to `gh-pages` to `wrangler r2 object put`, and
replace the CNAME. The manifests are signed, so the host doesn't need to be trusted.

## Updating

- Content facts used in several places (URLs, status, measured numbers) are in
  `src/data/site.ts`; the docs sidebar is `src/data/docs.ts`. A new page in `../docs`
  shows up once it's added to the glob in `src/content.config.ts`, to `DOC_NAV`, and
  to `DOC_PAGES` in `src/lib/doc-links.ts` (so links between docs point at it).
- Dependencies are pinned exactly; `pnpm outdated` then `pnpm up --latest`, and run
  `pnpm check && pnpm build && pnpm check:links` before committing.
- `security.txt` expires on 2027-10-23; bump `Expires` before then.
