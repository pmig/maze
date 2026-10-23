// The latest Maze release, looked up once at build time.
//
// GitHub's `/releases/latest` skips prereleases, and every alpha is a prerelease (it's
// on the preview channel), so we ask the API for the newest published release
// instead and pick its assets by name. Without network access, without a release, or
// while the repository is private, every link falls back to the releases page. The
// site is rebuilt after each release (see .github/workflows/website.yaml).

import { SITE } from "../data/site";

export type Arch = "aarch64" | "x86_64";

export interface ReleaseInfo {
  /** e.g. "0.1.0-alpha.1", or null when no release could be found. */
  version: string | null;
  /** The release page (or the list of releases). */
  page: string;
  prerelease: boolean;
  publishedAt: string | null;
  mac: Record<Arch, string>;
  linux: Record<Arch, string>;
}

interface GhAsset {
  name: string;
  browser_download_url: string;
}

interface GhRelease {
  tag_name: string;
  html_url: string;
  draft: boolean;
  prerelease: boolean;
  published_at: string | null;
  assets: GhAsset[];
}

const FALLBACK: ReleaseInfo = {
  version: null,
  page: SITE.releases,
  prerelease: true,
  publishedAt: null,
  mac: { aarch64: SITE.releases, x86_64: SITE.releases },
  linux: { aarch64: SITE.releases, x86_64: SITE.releases },
};

let cached: Promise<ReleaseInfo> | undefined;

export function getRelease(): Promise<ReleaseInfo> {
  cached ??= fetchRelease().catch(() => FALLBACK);
  return cached;
}

async function fetchRelease(): Promise<ReleaseInfo> {
  if (process.env.MAZE_SITE_OFFLINE === "1") return FALLBACK;

  const headers: Record<string, string> = {
    Accept: "application/vnd.github+json",
    "User-Agent": "mazemail.dev-build",
  };
  const token = process.env.GITHUB_TOKEN;
  if (token) headers.Authorization = `Bearer ${token}`;

  const api = SITE.repo.replace("https://github.com/", "https://api.github.com/repos/");
  const response = await fetch(`${api}/releases?per_page=10`, {
    headers,
    signal: AbortSignal.timeout(8000),
  });
  if (!response.ok) return FALLBACK;

  const releases = (await response.json()) as GhRelease[];
  const release = releases.find((r) => !r.draft && r.assets.some((a) => a.name.endsWith(".dmg")));
  if (!release) return FALLBACK;

  const asset = (suffix: string) =>
    release.assets.find((a) => a.name.endsWith(suffix))?.browser_download_url ?? release.html_url;

  return {
    version: release.tag_name.replace(/^v/, ""),
    page: release.html_url,
    prerelease: release.prerelease,
    publishedAt: release.published_at,
    mac: {
      aarch64: asset("-macos-aarch64.dmg"),
      x86_64: asset("-macos-x86_64.dmg"),
    },
    linux: {
      aarch64: asset("-linux-aarch64.tar.zst"),
      x86_64: asset("-linux-x86_64.tar.zst"),
    },
  };
}
