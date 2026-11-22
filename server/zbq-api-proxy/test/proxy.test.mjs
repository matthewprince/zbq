// node --test server/zbq-api-proxy/test  (no network: upstream fetch is mocked)
import assert from "node:assert/strict";
import test from "node:test";
import worker, { md5, lastfmSignature, safeImageUrl } from "../src/index.js";

const ZBQ = { "user-agent": "ZBQ/0.1.0 (+https://github.com/matthewprince/zbq)" };
const env = {
  LASTFM_API_KEY: "key123", LASTFM_SHARED_SECRET: "secret456",
  DISCOGS_KEY: "dk", DISCOGS_SECRET: "ds", TIDAL_CLIENT_ID: "tid", TIDAL_CLIENT_SECRET: "tsec",
};
const ctx = { waitUntil() {} };

function withFetch(handler, run) {
  const original = globalThis.fetch;
  const calls = [];
  globalThis.fetch = async (input, init = {}) => {
    calls.push({ url: String(input), init });
    return handler(String(input), init);
  };
  return run(calls).finally(() => { globalThis.fetch = original; });
}

test("md5 matches the RFC 1321 vectors", () => {
  assert.equal(md5(""), "d41d8cd98f00b204e9800998ecf8427e");
  assert.equal(md5("abc"), "900150983cd24fb0d6963f7d28e17f72");
  assert.equal(md5("The quick brown fox jumps over the lazy dog"), "9e107d9d372bb6826bd81d3542a419d6");
  assert.equal(md5("a".repeat(1000)), "cabe45dcc9ae5b66ba86600cca6b8ba8");
  assert.equal(md5("Björk"), "7dab4f23a40857b30fdd81370c211680", "UTF-8, as Python's hashlib computes it");
});

test("Last.fm signature: sorted name+value pairs, secret appended, format left out", () => {
  const params = { method: "auth.getSession", api_key: "key123", token: "tok", format: "json" };
  assert.equal(lastfmSignature(params, "secret456"), md5("api_keykey123methodauth.getSessiontokentok" + "secret456"));
});

test("only ZBQ clients are served", async () => {
  const response = await worker.fetch(new Request("https://p/lastfm/auth.getToken", { method: "POST", headers: { "user-agent": "QBZ/1.0.0" } }), env, ctx);
  assert.equal(response.status, 400);
  assert.equal((await worker.fetch(new Request("https://p/health"), env, ctx)).status, 200);
});

test("a scrobble is signed and POSTed; the session key is passed on, never echoed", async () => {
  await withFetch(async () => new Response(JSON.stringify({ scrobbles: { "@attr": { accepted: 1 } } })), async (calls) => {
    const response = await worker.fetch(new Request("https://p/lastfm/track.scrobble", {
      method: "POST", headers: ZBQ,
      body: JSON.stringify({ sk: "session", artist: "Artist", track: "Track", timestamp: "1700000000", api_key: "forged" }),
    }), env, ctx);
    assert.equal(response.status, 200);
    assert.equal(calls.length, 1);
    assert.equal(calls[0].init.method, "POST");
    const sent = new URLSearchParams(calls[0].init.body);
    assert.equal(sent.get("api_key"), "key123", "the client cannot choose the key");
    assert.equal(sent.get("method"), "track.scrobble");
    assert.equal(sent.get("format"), "json");
    const expected = {}; for (const [k, v] of sent) if (k !== "format" && k !== "api_sig") expected[k] = v;
    assert.equal(sent.get("api_sig"), lastfmSignature(expected, "secret456"));
  });
});

test("reads are plain GETs and unknown methods are refused", async () => {
  await withFetch(async () => new Response("{}"), async (calls) => {
    await worker.fetch(new Request("https://p/lastfm/user.getTopArtists", { method: "POST", headers: ZBQ, body: JSON.stringify({ user: "u", limit: 18 }) }), env, ctx);
    assert.match(calls[0].url, /^https:\/\/ws\.audioscrobbler\.com\/2\.0\/\?/);
    assert.equal(new URL(calls[0].url).searchParams.get("limit"), "18");
    assert.equal(new URL(calls[0].url).searchParams.get("api_sig"), null);
    const refused = await worker.fetch(new Request("https://p/lastfm/user.getFriends", { method: "POST", headers: ZBQ, body: "{}" }), env, ctx);
    assert.equal(refused.status, 404);
    assert.equal(calls.length, 1);
  });
});

test("auth.getToken comes back with Last.fm's own authorization page", async () => {
  await withFetch(async () => new Response(JSON.stringify({ token: "abc" })), async () => {
    const response = await worker.fetch(new Request("https://p/lastfm/auth.getToken", { method: "POST", headers: ZBQ, body: "{}" }), env, ctx);
    const data = await response.json();
    assert.equal(data.auth_url, "https://www.last.fm/api/auth/?api_key=key123&token=abc");
  });
});

test("Discogs: search and release are authorized, images only from Discogs", async () => {
  await withFetch(async (url) => new Response(url.includes("i.discogs.com") ? "img" : "{}", {
    headers: { "content-type": url.includes("i.discogs.com") ? "image/jpeg" : "application/json" },
  }), async (calls) => {
    await worker.fetch(new Request("https://p/discogs/search?q=a%20b&type=release&per_page=5&evil=1", { headers: ZBQ }), env, ctx);
    assert.equal(calls[0].url, "https://api.discogs.com/database/search?q=a+b&type=release&per_page=5");
    assert.equal(calls[0].init.headers.authorization, "Discogs key=dk, secret=ds");
    await worker.fetch(new Request("https://p/discogs/release/123", { headers: ZBQ }), env, ctx);
    assert.equal(calls[1].url, "https://api.discogs.com/releases/123");
    const image = await worker.fetch(new Request("https://p/discogs/image?url=" + encodeURIComponent("https://i.discogs.com/x.jpg"), { headers: ZBQ }), env, ctx);
    assert.equal(image.status, 200);
    assert.equal(calls[2].init.headers.authorization, undefined, "no key sent with images");
    const outside = await worker.fetch(new Request("https://p/discogs/image?url=" + encodeURIComponent("https://evil.example/x.jpg"), { headers: ZBQ }), env, ctx);
    assert.equal(outside.status, 400);
    assert.equal(calls.length, 3);
  });
  assert.equal(safeImageUrl("http://i.discogs.com/x"), null);
  assert.equal(safeImageUrl("not a url"), null);
});

test("unconfigured services answer 503 instead of failing upstream", async () => {
  for (const path of ["/lastfm/auth.getToken", "/discogs/search?q=x", "/tidal/token"]) {
    const response = await worker.fetch(new Request("https://p" + path, { method: path.startsWith("/lastfm") ? "POST" : "GET", headers: ZBQ, body: path.startsWith("/lastfm") ? "{}" : undefined }), {}, ctx);
    assert.equal(response.status, 503, path);
  }
});

test("Tidal token: client credentials, only the token goes back", async () => {
  await withFetch(async () => new Response(JSON.stringify({ access_token: "T", expires_in: 86400, scope: "x" })), async (calls) => {
    const response = await worker.fetch(new Request("https://p/tidal/token", { headers: ZBQ }), env, ctx);
    assert.deepEqual(Object.keys(await response.json()).sort(), ["access_token", "expires_in"]);
    assert.equal(calls[0].init.headers.authorization, "Basic " + btoa("tid:tsec"));
    assert.equal(calls[0].init.body, "grant_type=client_credentials");
  });
});
