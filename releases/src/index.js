// The `releases` Worker: serves Maze's signed update manifests on releases.mazemail.dev.
//
// The updater fetches `https://releases.mazemail.dev/<channel>/<os>-<arch>.json`
// (crates/auto_update/src/manifest.rs). The release workflow writes each manifest to
// the KV namespace bound as MANIFESTS under that path without the leading slash, e.g.
// `preview/macos-aarch64.json`. The downloads themselves stay on GitHub Releases.
//
// The manifests are signed (Ed25519) and verified by the app, so this Worker doesn't
// have to be trusted with their integrity. It reads nothing from the request but the
// method and path, logs nothing and sets no cookies.

/** `/<channel>/<os>-<arch>.json`; channels as in `ReleaseChannel::name()` (no `dev`). */
const MANIFEST_PATH = /^\/(stable|preview)\/(macos|linux)-(aarch64|x86_64)\.json$/;

/** Short, so a new release or a rollback reaches clients within minutes. */
const MANIFEST_CACHE_CONTROL = "public, max-age=60";

/**
 * Maps a request to what the Worker should do, without touching KV.
 *
 * @param {string} method
 * @param {string} pathname the URL path, already normalized by `new URL()`
 * @returns {{ kind: "manifest", key: string } | { kind: "not-found" } | { kind: "method-not-allowed" }}
 */
export function route(method, pathname) {
  if (method !== "GET" && method !== "HEAD") {
    return { kind: "method-not-allowed" };
  }
  if (!MANIFEST_PATH.test(pathname)) {
    return { kind: "not-found" };
  }
  return { kind: "manifest", key: pathname.slice(1) };
}

/**
 * @param {number} status
 * @param {string} body
 * @param {Record<string, string>} [extra]
 */
function text(status, body, extra = {}) {
  return new Response(body, {
    status,
    headers: {
      "Content-Type": "text/plain; charset=utf-8",
      "Cache-Control": "no-store",
      "X-Content-Type-Options": "nosniff",
      ...extra,
    },
  });
}

/**
 * @param {Request} request
 * @param {{ MANIFESTS: { get(key: string): Promise<string | null> } }} env
 * @returns {Promise<Response>}
 */
export async function handle(request, env) {
  const target = route(request.method, new URL(request.url).pathname);
  if (target.kind === "method-not-allowed") {
    return text(405, "Method not allowed\n", { Allow: "GET, HEAD" });
  }
  if (target.kind === "not-found") {
    return text(404, "Not found\n");
  }

  let manifest;
  try {
    manifest = await env.MANIFESTS.get(target.key);
  } catch {
    // The updater treats any failure as "no update right now" and tries again later.
    return text(503, "Service unavailable\n", { "Retry-After": "60" });
  }
  if (manifest === null) {
    // A platform or channel without a release yet.
    return text(404, "Not found\n");
  }

  return new Response(request.method === "HEAD" ? null : manifest, {
    status: 200,
    headers: {
      "Content-Type": "application/json",
      "Cache-Control": MANIFEST_CACHE_CONTROL,
      "X-Content-Type-Options": "nosniff",
    },
  });
}

export default {
  /** @param {Request} request @param {any} env */
  fetch(request, env) {
    return handle(request, env);
  },
};
