# Design brief: mazemail.dev

The website for Maze should feel like the app: calm, dense, fast, keyboard-first. This
brief records what we looked at, what we take from it, and the rules the site is built
to. It is a working document for whoever touches the site next.

## What we studied (October 2026)

### zed.dev

- **Frame and grid.** Content sits in one centered column whose left and right edges are
  drawn as thin hairlines running the whole page; sections are separated by horizontal
  hairlines, so the page reads like a blueprint or an editor's pane layout. Few cards,
  few shadows.
- **Sticky top bar** (~57 px) with a bottom border, short nav labels (Product,
  Resources, Extensions, Docs, Pricing) and **keyboard hints** printed next to the
  actions ("Download D"), which sets a keyboard-first tone before you scroll.
- **Typography.** A serif display face (IBM Plex Serif, italic for the hero line), a
  humanist sans for running text (iA Writer Quattro), and Lilex, a Plex-derived mono, for
  code and labels. Headings are modest in weight; hierarchy comes from size and spacing,
  not from color.
- **Color.** Near-monochrome: a barely warm off-white in light mode, a very dark
  blue-gray (`hsl(218 13% 7.5%)`) in dark mode, and a single saturated blue accent used
  sparingly (hero line, links, one solid footer block). Dark mode is first class.
- **Product first.** Large product imagery directly under the hero; feature sections
  alternate a short serif heading, two or three sentences, and a concrete UI excerpt.
  Copy is technical and concrete rather than superlative.

### delta.dev

- Built with **Astro**, also framed: the header is a bordered, rounded strip inside the
  frame; columns have vertical borders (`border-x`) and the page background differs
  slightly from the content background, so the frame is visible without decoration.
- A small **"Now in Beta" label** in the nav states maturity honestly.
- The hero is a **live-looking product excerpt** (editor file headers, code, inline
  comments with names), not an illustration: the product explains itself.
- **Dense, small headings.** Section titles are often `text-lg` (18 px) and
  `1.375rem`; body copy around 15–16 px. It feels like documentation that happens to be
  a landing page. Tables, code and metrics ("41.2µs to 3.4µs") carry the persuasion.
- Clear nav to **Download, Docs, Roadmap, Blog**, and a plain one-line footer.
- Fonts again Plex/iA Quattro/Lilex, with a warm cream neutral scale and one brand hue
  defined in OKLCH and derived with `color-mix()`.

## What Maze takes

| From          | We take                                                                                         | We do differently                                                                 |
| ------------- | ----------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------- |
| Zed           | The hairline frame, sticky bar with kbd hints, monochrome chrome, one accent, big screenshot     | No serif display face: Maze's headings use the sans, tight and medium weight       |
| Zed           | Concrete, measured claims instead of adjectives                                                  | Every number links to where it comes from                                          |
| delta         | An honest maturity label ("Pre-alpha") in the header                                            | Ours also says what doesn't work yet, on the home page                             |
| delta         | Small, dense section headings; tables and terminal output as the main "illustrations"           | No avatars or testimonials: there are no users to quote yet                        |
| delta         | OKLCH-ish tokens with `color-mix()` derived tints                                               | Monochrome graphite instead of red-orange; amber only for the "goal" dot     |
| Both          | Self-hosted Plex-family type and Lilex for code                                                  | Two families only (IBM Plex Sans + Lilex), latin subsets, no CDN                   |

Nothing is copied: no assets, logos, copy, or CSS from either site.

## Maze's rules

- **Palette.** Graphite: neutral grays and a monochrome accent (near-black `#18181B` in
  light, near-white `#F4F4F5` in dark) for links, the primary button and focus rings.
  Amber (`#FFB86B`) is the logo's colour: the app icon is an amber tile (`#FFC888` →
  `#F5A04A`) with a graphite envelope, and the goal in the fold of its flap shows the
  amber through. Outside the logo, amber appears only as a tiny marker: the pre-alpha
  dot, list bullets for "done". Both themes meet WCAG AA for text (most
  pairs AAA).
- **Themes.** Follow `prefers-color-scheme` by default. A toggle in the header cycles
  system → light → dark and remembers the choice in `localStorage` (not a cookie). A
  tiny blocking script sets the theme before first paint, so there is no flash.
- **Type.** IBM Plex Sans Variable for everything readable, Lilex Variable for code,
  keyboard keys, labels and numbers. Base size 16 px, line height 1.6 for prose, 1.25
  for headings, `text-wrap: balance` on headings.
- **Layout.** One 1200 px frame with hairline borders, sections divided by hairlines,
  12-column-ish grids that collapse to one column below 760 px. Generous outer
  whitespace, dense insides.
- **Imagery.** Real screenshots of Maze only (from `docs/screenshots/`), framed like a
  window, served as AVIF/WebP at several widths. No stock art, no illustrations. The
  one exception is the dot animation next to the hero text, built from the logo and
  simple icons for what Maze does (mail, search, speed, encryption, keys, CLI, agents).
  The other is Maze's own window on the home page, rebuilt in HTML and CSS the way
  zed.dev shows its editor: title bar, spaces bar, folder pane, message list, message and
  the agent column on the right, laid out after `crates/mail_ui` and `crates/agent_ui`
  in Maze's default One Light / One Dark colors (taken from `assets/themes/one.json`,
  following the site's theme) and the system UI font, like the app. It sits directly
  under the hero text and is the home page's only picture of the app (it replaced the
  real screenshots there; docs pages may still use them). The window has
  made-up mail; agent tasks play in its agent column (prompt, `mazemail` commands,
  proposal, Accept) and the mailbox reacts: matching rows light up, then leave the list
  or get a star, folder counts change. It renders at a fixed design size (1200 × 720)
  and is scaled to its column with CSS alone (container query units and
  `tan(atan2())` for the ratio), so its height is fixed before anything loads and
  nothing shifts. Narrower columns get smaller design sizes: without the message view
  (800 × 720), and on phones the list above the agent column (420 × 840).
- **Tone.** Short sentences, concrete nouns, no hype. Say what exists; label everything
  else "planned". The product is "Maze"; `mazemail` appears only as a technical name
  (CLI, domain, repository).
- **Weight.** No client-side framework. JavaScript is one small file for the theme
  toggle, keyboard shortcuts and naming the visitor's computer (copy says "device" and
  becomes "Mac", "Linux PC" or "Windows PC"; nothing is sent anywhere), plus `dots.js`
  (about 15 KB, 5 KB compressed, home page only) for the hero animation: one canvas,
  paused off screen and in background tabs, a still logo under
  `prefers-reduced-motion`, hidden below 1000 px. And `maze-window.js` (about 12 KB, 4 KB
  compressed, home page only) for the HTML Maze window under the hero: four made-up
  agent tasks, typed out, run, proposed and accepted step by step while the mailbox
  reacts, paused off screen, in background tabs and with its Pause button. Under
  `prefers-reduced-motion` each task shows still at its proposal; without JavaScript the
  first one does. The window is `aria-hidden` with nothing focusable inside and a text
  list of the same tasks for screen readers; tasks that aren't shipped carry a "Soon"
  badge. Its markup is about 44 KB (7 KB compressed) with one inline icon sprite and no
  images. Its classes start with `mw-`, never `ad-`, which ad blockers hide. The site
  works fully without any of them.
- **Privacy.** No analytics, no cookies, no third-party requests of any kind, enforced
  by the Content Security Policy. The footer says so.
- **Accessibility.** Semantic landmarks, one `h1` per page, skip link, visible focus,
  keyboard reachable everything, `prefers-reduced-motion` respected, alt text that
  describes what the screenshot shows.
