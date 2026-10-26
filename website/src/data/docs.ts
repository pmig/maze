// Sidebar order and short titles for the docs pages. The content itself comes from
// the repository's docs/ folder (src/content.config.ts).

export interface DocNavItem {
  id: string;
  title: string;
  description: string;
}

export const DOC_NAV: { group: string; items: DocNavItem[] }[] = [
  {
    group: "Using Maze",
    items: [
      {
        id: "getting-started",
        title: "Getting started",
        description: "Install, accounts, import, the window, writing mail, settings, updates and shortcuts.",
      },
      {
        id: "agents",
        title: "Agents",
        description: "The agent column: Claude Code, Codex, Gemini CLI, Cursor and pi, sign-in, the sandbox, proposals.",
      },
      {
        id: "security",
        title: "Security",
        description: "Encryption at rest, where the key lives, recovery, API tokens.",
      },
    ],
  },
  {
    group: "Command line",
    items: [
      {
        id: "cli",
        title: "The mazemail CLI",
        description: "How the CLI signs in: tokens, profiles, approvals, headless mode.",
      },
      {
        id: "agents-cli",
        title: "CLI reference",
        description: "Every command, option and JSON shape. Generated from the command registry.",
      },
    ],
  },
];

export const DOC_ORDER: DocNavItem[] = DOC_NAV.flatMap((g) => g.items);
