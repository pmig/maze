// @ts-check
import { defineConfig, fontProviders } from "astro/config";
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
  // Adds a CSP <meta> with hashes for the inline <style> that <Font> emits. The rest of
  // the policy is in public/_headers; the browser enforces both.
  security: {
    csp: {
      styleDirective: {
        resources: ["'self'", { resource: "'unsafe-inline'", kind: "attribute" }],
      },
    },
  },
  devToolbar: { enabled: false },
  prefetch: false,
  // From the @fontsource-variable packages (SIL Open Font License 1.1), at the installed
  // version. Astro copies the files into _astro/, so the site stays self-hosted.
  fonts: [
    {
      name: "IBM Plex Sans Variable",
      cssVariable: "--font-sans",
      provider: fontProviders.npm(),
      weights: ["100 700"],
      styles: ["normal"],
      fallbacks: ["ui-sans-serif", "system-ui", "-apple-system", "Segoe UI", "Roboto", "sans-serif"],
    },
    // The italic faces are in a separate stylesheet; same name and variable, so Astro
    // merges them into the family above.
    {
      name: "IBM Plex Sans Variable",
      cssVariable: "--font-sans",
      provider: fontProviders.npm(),
      options: { file: "wght-italic.css" },
      weights: ["100 700"],
      fallbacks: ["ui-sans-serif", "system-ui", "-apple-system", "Segoe UI", "Roboto", "sans-serif"],
    },
    {
      name: "Lilex Variable",
      cssVariable: "--font-mono",
      provider: fontProviders.npm(),
      weights: ["100 700"],
      styles: ["normal"],
      fallbacks: ["ui-monospace", "SF Mono", "Menlo", "Consolas", "monospace"],
    },
  ],
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
