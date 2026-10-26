// Facts the site repeats in several places. Keep them in one file so a change (a new
// requirement, a new measurement) is made once.

export const SITE = {
  name: "Maze",
  url: "https://mazemail.dev",
  description:
    "Maze is the agentic email client: local-first, open source and written in Rust, crafted for speed and collaboration with humans and AI: GPU-rendered, encrypted at rest, with sandboxed AI agents next to your mail and scoped access through the mazemail CLI.",
  repo: "https://github.com/pmig/mazemail",
  releases: "https://github.com/pmig/mazemail/releases",
  issues: "https://github.com/pmig/mazemail/issues",
  author: "Philip Miglinci",
  license: "GPL-3.0-or-later",
  status: "Pre-alpha",
  macosMinimum: "macOS 13",
} as const;

/** A link to a file in the repository on GitHub (default branch). */
export function repoFile(path: string): string {
  return `${SITE.repo}/blob/main/${path.replace(/^\/+/, "")}`;
}

/**
 * Measured numbers shown on the home page. Each one names its source, so a reader
 * (and the next editor of this file) can check it. Update them when the
 * benchmarks change; the budgets are the hard targets from the design spec.
 */
export const NUMBERS = [
  {
    value: "~130",
    unit: "ms",
    label: "start to the message list",
    detail: "Measured from the app bundle on Apple Silicon (warm start; first launch after boot ~370 ms). Budget: under 300 ms.",
  },
  {
    value: "1M",
    unit: "messages",
    label: "in one folder, opened in 6 ms",
    detail: "First 200k thread keys of a 1M-thread folder in 6 ms, all 1M in 36 ms, encryption on.",
  },
  {
    value: "~12",
    unit: "ms",
    label: "search p95 over 1M messages",
    detail: "Gmail-style queries over the encrypted Tantivy index, 1M-message benchmark. Budget: under 100 ms.",
  },
  {
    value: "~123",
    unit: "MB",
    label: "the whole app, one binary",
    detail: "Stripped macOS release binary with the embedded Servo engine for original HTML mail (ADR-020); about 49 MB without it. The UI itself is native GPUI.",
  },
] as const;
