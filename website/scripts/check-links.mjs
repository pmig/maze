#!/usr/bin/env node
// Checks that every internal link and asset reference in the built site resolves to a
// file in dist/, and that every #fragment points at an id on the target page.
// Usage: node scripts/check-links.mjs dist   (no dependencies)

import { readFile, readdir, stat } from "node:fs/promises";
import path from "node:path";

const root = path.resolve(process.argv[2] ?? "dist");
const SITE = "https://mazemail.dev";

async function walk(dir) {
  const out = [];
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) out.push(...(await walk(full)));
    else out.push(full);
  }
  return out;
}

async function exists(file) {
  try {
    return (await stat(file)).isFile();
  } catch {
    return false;
  }
}

/** Maps a URL path to the file Cloudflare would serve for it. */
async function resolve(urlPath) {
  const clean = decodeURIComponent(urlPath);
  const candidates = clean.endsWith("/")
    ? [path.join(root, clean, "index.html")]
    : [path.join(root, clean), path.join(root, clean, "index.html"), path.join(root, `${clean}.html`)];
  for (const c of candidates) if (await exists(c)) return c;
  return null;
}

const redirects = new Set(
  (await readFile(path.join(root, "_redirects"), "utf8").catch(() => ""))
    .split("\n")
    .filter((l) => l.trim() && !l.startsWith("#"))
    .map((l) => l.trim().split(/\s+/)[0]),
);

const ids = new Map();
async function idsOf(file) {
  if (!ids.has(file)) {
    const html = await readFile(file, "utf8");
    ids.set(file, new Set([...html.matchAll(/\sid="([^"]+)"/g)].map((m) => m[1])));
  }
  return ids.get(file);
}

const files = (await walk(root)).filter((f) => f.endsWith(".html"));
const problems = [];
let checked = 0;

for (const file of files) {
  const html = await readFile(file, "utf8");
  const refs = [
    ...html.matchAll(/\s(?:href|src)="([^"]+)"/g),
    ...[...html.matchAll(/\ssrcset="([^"]+)"/g)].flatMap((m) =>
      m[1].split(",").map((part) => [null, part.trim().split(/\s+/)[0]]),
    ),
  ].map((m) => m[1]);

  for (let ref of refs) {
    if (!ref || /^(mailto:|tel:|data:|javascript:)/.test(ref)) continue;
    if (ref.startsWith(SITE)) ref = ref.slice(SITE.length) || "/";
    if (/^[a-z]+:\/\//i.test(ref) || ref.startsWith("//")) continue;
    checked++;

    const [pathPart, fragment] = ref.split("#");
    let target = file;
    if (pathPart) {
      const absolute = pathPart.startsWith("/")
        ? pathPart
        : path.posix.join("/", path.relative(root, path.dirname(file)).split(path.sep).join("/"), pathPart);
      if (redirects.has(absolute)) continue;
      target = await resolve(absolute);
      if (!target) {
        problems.push(`${path.relative(root, file)}: broken link ${ref}`);
        continue;
      }
    }
    if (fragment && target.endsWith(".html") && !(await idsOf(target)).has(decodeURIComponent(fragment))) {
      problems.push(`${path.relative(root, file)}: missing anchor ${ref}`);
    }
  }
}

if (problems.length) {
  console.error(problems.join("\n"));
  console.error(`\n${problems.length} problem(s) in ${checked} internal references.`);
  process.exit(1);
}
console.log(`All ${checked} internal references in ${files.length} pages resolve.`);
