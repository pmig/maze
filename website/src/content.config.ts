import { defineCollection } from "astro:content";
import { glob } from "astro/loaders";

// The user docs are the Markdown files in the repository's docs/ folder, read in place,
// so the site can never drift from them. Their order, short titles and sidebar
// groups are in src/data/docs.ts. `releasing.md` (maintainers) and the ADRs stay
// on GitHub.
const docs = defineCollection({
  loader: glob({
    base: "../docs",
    pattern: ["getting-started.md", "agents.md", "cli.md", "AGENTS-cli.md", "security.md"],
    generateId: ({ entry }) => entry.replace(/\.md$/, "").toLowerCase(),
  }),
});

export const collections = { docs };
