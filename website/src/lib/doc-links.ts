// Sätteri plugins for the repository's Markdown docs.
//
// The docs are written for GitHub, so they link to each other as `cli.md#tokens`, to
// ADRs as `adr/0010-auto-update.md`, and to repository files as `../SECURITY.md`. On
// the site, docs that have a page link to that page; everything else in the repository
// links to GitHub. Tables get a scroll wrapper so wide ones don't break the layout.
//
// The docs also use placeholders like `<previous version>` or `move:<folder-id>`
// outside code spans. Markdown reads them as raw HTML; `literalPlaceholders` shows
// anything that isn't a real HTML element as the text it is.

import path from "node:path";
import { fileURLToPath } from "node:url";
import { defineHastPlugin, defineMdastPlugin } from "satteri";
import { repoFile } from "../data/site";

/** Repository-relative Markdown files that have a page on the site. */
export const DOC_PAGES: Record<string, string> = {
  "docs/getting-started.md": "/docs/getting-started/",
  "docs/agents.md": "/docs/agents/",
  "docs/cli.md": "/docs/cli/",
  "docs/AGENTS-cli.md": "/docs/agents-cli/",
  "docs/security.md": "/docs/security/",
  "CHANGELOG.md": "/changelog/",
};

const REPO_ROOT = fileURLToPath(new URL("../../../", import.meta.url));
const EXTERNAL = /^(?:[a-z][a-z0-9+.-]*:|\/\/)/i;

export function rewriteHref(href: string, fileURL: URL | undefined): string {
  if (!href || href.startsWith("#") || EXTERNAL.test(href)) return href;

  const hashAt = href.indexOf("#");
  const target = hashAt === -1 ? href : href.slice(0, hashAt);
  const hash = hashAt === -1 ? "" : href.slice(hashAt);

  const fromDir = fileURL ? path.dirname(fileURLToPath(fileURL)) : path.join(REPO_ROOT, "docs");
  const absolute = path.resolve(fromDir, target);
  const relative = path.relative(REPO_ROOT, absolute).split(path.sep).join("/");
  if (relative.startsWith("..")) return href;

  const page = DOC_PAGES[relative];
  if (page) return page + hash;

  // A directory (e.g. `adr/`) is shown as a tree on GitHub.
  const isDir = target.endsWith("/");
  const url = isDir ? repoFile(relative).replace("/blob/", "/tree/") : repoFile(relative);
  return url + hash;
}

export const docLinks = defineHastPlugin({
  name: "maze-doc-links",
  element: {
    filter: ["a", "table"],
    visit(node, ctx) {
      if (node.tagName === "a") {
        const href = node.properties?.href;
        if (typeof href !== "string") return;
        const next = rewriteHref(href, ctx.fileURL);
        if (next !== href) ctx.setProperty(node, "href", next);
        return;
      }
      ctx.wrapNode(node, {
        type: "element",
        tagName: "div",
        properties: { className: ["table-wrap"] },
        children: [],
      });
    },
  },
});

const HTML_ELEMENTS = new Set(
  (
    "a abbr address article aside b bdi bdo blockquote br caption cite code col colgroup " +
    "dd del details dfn div dl dt em figcaption figure footer h1 h2 h3 h4 h5 h6 header hr i " +
    "img ins kbd li mark ol p picture pre q s samp section small source span strong sub " +
    "summary sup table tbody td tfoot th thead time tr u ul var video wbr"
  ).split(" "),
);
const TAG = /^<\/?([A-Za-z][A-Za-z0-9-]*)(?:\s[^>]*)?\/?>$/;

export const literalPlaceholders = defineMdastPlugin({
  name: "maze-literal-placeholders",
  html(node) {
    const match = TAG.exec(node.value.trim());
    if (match?.[1] && !HTML_ELEMENTS.has(match[1].toLowerCase())) {
      return { type: "text", value: node.value };
    }
    return undefined;
  },
});
