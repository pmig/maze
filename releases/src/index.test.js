// Run with `pnpm test` (node's built-in test runner; Request and Response are globals
// in Node 24, so the fetch handler runs here against an in-memory KV stand-in).
import assert from "node:assert/strict";
import { test } from "node:test";

import { handle, route } from "./index.js";

const MANIFEST = '{"version":"0.1.0-alpha.1"}\n';

function kv(entries = {}) {
  return {
    async get(key) {
      return Object.hasOwn(entries, key) ? entries[key] : null;
    },
  };
}

function request(path, method = "GET") {
  return new Request(`https://releases.mazemail.dev${path}`, { method });
}

test("route accepts every channel and platform the updater asks for", () => {
  for (const channel of ["stable", "preview"]) {
    for (const platform of ["macos-aarch64", "macos-x86_64", "linux-x86_64", "linux-aarch64"]) {
      const path = `/${channel}/${platform}.json`;
      assert.deepEqual(route("GET", path), { kind: "manifest", key: path.slice(1) });
      assert.deepEqual(route("HEAD", path), { kind: "manifest", key: path.slice(1) });
    }
  }
});

test("route rejects anything else", () => {
  for (const path of [
    "/",
    "/stable",
    "/stable/",
    "/dev/macos-aarch64.json",
    "/stable/windows-x86_64.json",
    "/stable/macos-arm64.json",
    "/stable/macos-aarch64.json/",
    "/stable/macos-aarch64.JSON",
    "/Stable/macos-aarch64.json",
    "/stable/macos-aarch64",
    "//stable/macos-aarch64.json",
    "/stable/macos%2Daarch64.json",
    "/stable/macos-aarch64.json.bak",
    "/x/stable/macos-aarch64.json",
  ]) {
    assert.deepEqual(route("GET", path), { kind: "not-found" }, path);
  }
  for (const method of ["POST", "PUT", "DELETE", "OPTIONS", "PATCH"]) {
    assert.deepEqual(route(method, "/stable/macos-aarch64.json"), { kind: "method-not-allowed" });
  }
});

test("GET serves the manifest from KV with short caching", async () => {
  const env = { MANIFESTS: kv({ "preview/macos-aarch64.json": MANIFEST }) };
  const response = await handle(request("/preview/macos-aarch64.json?x=1"), env);
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("content-type"), "application/json");
  assert.equal(response.headers.get("cache-control"), "public, max-age=60");
  assert.equal(response.headers.get("x-content-type-options"), "nosniff");
  assert.equal(response.headers.get("set-cookie"), null);
  assert.equal(await response.text(), MANIFEST);
});

test("HEAD has the headers but no body", async () => {
  const env = { MANIFESTS: kv({ "stable/linux-x86_64.json": MANIFEST }) };
  const response = await handle(request("/stable/linux-x86_64.json", "HEAD"), env);
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("content-type"), "application/json");
  assert.equal(response.body, null);
});

test("a manifest that isn't published yet is a 404", async () => {
  const response = await handle(request("/stable/macos-x86_64.json"), { MANIFESTS: kv() });
  assert.equal(response.status, 404);
  assert.equal(response.headers.get("cache-control"), "no-store");
});

test("unknown paths are a 404 without asking KV", async () => {
  const env = {
    MANIFESTS: {
      async get() {
        throw new Error("KV must not be read");
      },
    },
  };
  for (const path of ["/", "/favicon.ico", "/stable/../preview/x.json"]) {
    const response = await handle(request(path), env);
    assert.equal(response.status, 404, path);
    assert.match(response.headers.get("content-type"), /^text\/plain/);
  }
});

test("other methods are a 405 with Allow", async () => {
  const response = await handle(request("/stable/macos-aarch64.json", "POST"), { MANIFESTS: kv() });
  assert.equal(response.status, 405);
  assert.equal(response.headers.get("allow"), "GET, HEAD");
});

test("a KV failure is a 503 that isn't cached", async () => {
  const env = {
    MANIFESTS: {
      async get() {
        throw new Error("boom");
      },
    },
  };
  const response = await handle(request("/stable/macos-aarch64.json"), env);
  assert.equal(response.status, 503);
  assert.equal(response.headers.get("cache-control"), "no-store");
});
