// @ts-check
import { defineConfig } from "astro/config";
import sitemap from "@astrojs/sitemap";
import { satteri } from "@astrojs/markdown-satteri";
import { docLinks, literalPlaceholders } from "./src/lib/doc-links.ts";

// Fully static: `astro build` writes plain files to dist/, which Cloudflare serves as
// Workers static assets (wrangler.jsonc). No adapter, no server code.
export default defineConfig({
  site: "https://mazemail.dev",
  output: "static",
  // Pages are directories (/docs/cli/index.html), served at /docs/cli/, which is
  // also what Cloudflare's default `auto-trailing-slash` HTML handling expects.
  trailingSlash: "always",
  build: {
    format: "directory",
    // Never inline CSS: the Content Security Policy allows only same-origin
    // stylesheets (public/_headers).
    inlineStylesheets: "never",
  },
  devToolbar: { enabled: false },
  prefetch: false,
  integrations: [
    sitemap({
      filter: (page) => !page.endsWith("/404/"),
    }),
  ],
  markdown: {
    processor: satteri({
      mdastPlugins: [literalPlaceholders],
      hastPlugins: [docLinks],
    }),
    shikiConfig: {
      themes: { light: "github-light", dark: "github-dark-dimmed" },
      defaultColor: false,
      wrap: false,
    },
  },
  vite: {
    server: {
      // The docs and screenshots live in the repository, one level up.
      fs: { allow: [".."] },
    },
  },
});
