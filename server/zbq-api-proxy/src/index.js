// ZBQ's API proxy (Cloudflare Worker): the services ZBQ uses that need a key
// the app must not ship. Keys live in Worker secrets:
//   LASTFM_API_KEY, LASTFM_SHARED_SECRET    Last.fm (scrobbling, stats, similar)
//   DISCOGS_KEY, DISCOGS_SECRET             Discogs (artwork, artist search)
//   TIDAL_CLIENT_ID, TIDAL_CLIENT_SECRET    Tidal app token (music links, playlist import)
// A service whose keys are not set answers 503 and the app falls back or hides it.
//
// Only ZBQ clients (User-Agent "ZBQ/...") are served, only the calls ZBQ makes
// are forwarded, and nothing a user sends (Last.fm session keys included) is
// logged or stored. Per-IP limits sit at the zone edge, before the Worker runs.

const LASTFM_API = "https://ws.audioscrobbler.com/2.0/";
const DISCOGS_API = "https://api.discogs.com";
const TIDAL_TOKEN_URL = "https://auth.tidal.com/v1/oauth2/token";
const UPSTREAM_UA = "ZBQ-api-proxy/1.0 +https://github.com/matthewprince/zbq";

// The Last.fm methods ZBQ calls, and which of them Last.fm wants signed.
const LASTFM_METHODS = new Set([
  "auth.getToken", "auth.getSession", "track.scrobble", "track.updateNowPlaying",
  "artist.getInfo", "artist.getSimilar", "artist.getTopAlbums", "album.getInfo",
  "track.getSimilar", "user.getLovedTracks", "user.getRecentTracks",
  "user.getTopAlbums", "user.getTopArtists", "user.getTopTracks",
]);
const LASTFM_SIGNED = new Set(["auth.getToken", "auth.getSession", "track.scrobble", "track.updateNowPlaying"]);

const IMAGE_HOSTS = new Set(["i.discogs.com", "img.discogs.com"]);
const IMAGE_MAX_BYTES = 15 * 1024 * 1024;

export default {
  async fetch(request, env, ctx) {
    const url = new URL(request.url);
    if (url.pathname === "/health") return json({ ok: true });
    const agent = request.headers.get("user-agent") || "";
    if (!agent.startsWith("ZBQ/")) return json({ error: "This proxy is only for ZBQ clients." }, 400);
    const [, service = "", ...rest] = url.pathname.split("/");
    try {
      if (service === "lastfm") return await lastfm(request, env, rest.join("/"), url);
      if (service === "discogs") return await discogs(request, env, ctx, rest, url);
      if (service === "tidal" && rest.join("/") === "token") return await tidalToken(env);
      return json({ error: "Not found" }, 404);
    } catch (error) {
      return json({ error: "Upstream request failed" }, 502);
    }
  },
};

// ---- Last.fm -----------------------------------------------------------------

async function lastfm(request, env, method, url) {
  if (!LASTFM_METHODS.has(method)) return json({ error: 6, message: "Unsupported method" }, 404);
  if (!env.LASTFM_API_KEY || !env.LASTFM_SHARED_SECRET) return json({ error: 16, message: "Last.fm is not configured" }, 503);
  const params = {};
  for (const [key, value] of url.searchParams) params[key] = value;
  if (request.method === "POST") {
    const body = await request.json().catch(() => ({}));
    for (const [key, value] of Object.entries(body || {})) {
      if (value !== null && value !== undefined) params[key] = String(value);
    }
  }
  for (const reserved of ["api_key", "api_sig", "method", "format", "callback"]) delete params[reserved];
  params.method = method;
  params.api_key = env.LASTFM_API_KEY;
  if (LASTFM_SIGNED.has(method)) params.api_sig = lastfmSignature(params, env.LASTFM_SHARED_SECRET);
  const form = new URLSearchParams({ ...params, format: "json" });
  const upstream = LASTFM_SIGNED.has(method)
    ? await fetch(LASTFM_API, { method: "POST", headers: { "user-agent": UPSTREAM_UA, "content-type": "application/x-www-form-urlencoded" }, body: form })
    : await fetch(`${LASTFM_API}?${form}`, { headers: { "user-agent": UPSTREAM_UA } });
  const text = await upstream.text();
  if (method === "auth.getToken" && upstream.ok) {
    // The app sends the user to Last.fm's own page to allow ZBQ.
    try {
      const data = JSON.parse(text);
      if (data.token) {
        data.auth_url = `https://www.last.fm/api/auth/?api_key=${encodeURIComponent(env.LASTFM_API_KEY)}&token=${encodeURIComponent(data.token)}`;
        return json(data, 200);
      }
    } catch {}
  }
  return new Response(text, { status: upstream.status, headers: { "content-type": "application/json", "cache-control": "no-store" } });
}

/// Last.fm's api_sig: every parameter but format/callback, sorted by name,
/// concatenated as name+value, the shared secret appended, MD5 of the UTF-8.
export function lastfmSignature(params, secret) {
  const base = Object.keys(params)
    .filter((key) => key !== "format" && key !== "callback" && key !== "api_sig")
    .sort()
    .map((key) => key + params[key])
    .join("");
  return md5(base + secret);
}

// ---- Discogs -----------------------------------------------------------------

async function discogs(request, env, ctx, rest, url) {
  if (request.method !== "GET") return json({ error: "Method not allowed" }, 405);
  const kind = rest[0] || "";
  let upstream;
  let ttl;
  if (kind === "image") {
    const target = safeImageUrl(url.searchParams.get("url"));
    if (!target) return json({ error: "Only Discogs images" }, 400);
    upstream = target;
    ttl = 7 * 86400;
  } else {
    if (!env.DISCOGS_KEY || !env.DISCOGS_SECRET) return json({ message: "Discogs is not configured" }, 503);
    if (kind === "search") {
      const query = new URLSearchParams();
      for (const name of ["q", "type", "per_page", "page", "artist", "release_title"]) {
        const value = url.searchParams.get(name);
        if (value !== null) query.set(name, value);
      }
      upstream = `${DISCOGS_API}/database/search?${query}`;
      ttl = 3600;
    } else if (kind === "release" && /^\d+$/.test(rest[1] || "")) {
      upstream = `${DISCOGS_API}/releases/${rest[1]}`;
      ttl = 86400;
    } else {
      return json({ error: "Not found" }, 404);
    }
  }
  const cache = globalThis.caches ? caches.default : null;
  const cacheKey = new Request(`https://zbq-proxy.cache/${encodeURIComponent(upstream)}`);
  if (cache) {
    const hit = await cache.match(cacheKey);
    if (hit) return hit;
  }
  const headers = { "user-agent": UPSTREAM_UA };
  if (kind !== "image") headers.authorization = `Discogs key=${env.DISCOGS_KEY}, secret=${env.DISCOGS_SECRET}`;
  const answer = await fetch(upstream, { headers });
  if (kind === "image") {
    const length = Number(answer.headers.get("content-length") || "0");
    const type = answer.headers.get("content-type") || "";
    if (!answer.ok || !type.startsWith("image/") || length > IMAGE_MAX_BYTES) return json({ error: "Image unavailable" }, 502);
  }
  const response = new Response(answer.body, {
    status: answer.status,
    headers: {
      "content-type": answer.headers.get("content-type") || "application/json",
      "cache-control": answer.ok ? `public, max-age=${ttl}` : "no-store",
    },
  });
  if (cache && answer.ok) ctx.waitUntil(cache.put(cacheKey, response.clone()));
  return response;
}

export function safeImageUrl(value) {
  try {
    const target = new URL(value || "");
    return target.protocol === "https:" && IMAGE_HOSTS.has(target.hostname) ? target.toString() : null;
  } catch {
    return null;
  }
}

// ---- Tidal -------------------------------------------------------------------

async function tidalToken(env) {
  if (!env.TIDAL_CLIENT_ID || !env.TIDAL_CLIENT_SECRET) return json({ error: "Tidal is not configured" }, 503);
  const cache = globalThis.caches ? caches.default : null;
  const cacheKey = new Request("https://zbq-proxy.cache/tidal-token");
  if (cache) {
    const hit = await cache.match(cacheKey);
    if (hit) return hit;
  }
  const answer = await fetch(TIDAL_TOKEN_URL, {
    method: "POST",
    headers: {
      authorization: "Basic " + btoa(`${env.TIDAL_CLIENT_ID}:${env.TIDAL_CLIENT_SECRET}`),
      "content-type": "application/x-www-form-urlencoded",
      "user-agent": UPSTREAM_UA,
    },
    body: "grant_type=client_credentials",
  });
  if (!answer.ok) return json({ error: "Tidal token request failed" }, 502);
  const data = await answer.json();
  const lifetime = Math.max(60, Math.min(Number(data.expires_in) || 3600, 86400) - 120);
  const response = new Response(JSON.stringify({ access_token: data.access_token, expires_in: lifetime }), {
    headers: { "content-type": "application/json", "cache-control": `public, max-age=${lifetime}` },
  });
  if (cache) await cache.put(cacheKey, response.clone());
  return response;
}

// ---- helpers -----------------------------------------------------------------

function json(value, status = 200) {
  return new Response(JSON.stringify(value), { status, headers: { "content-type": "application/json", "cache-control": "no-store" } });
}

/// MD5 (RFC 1321) of a string's UTF-8 bytes, hex. Workers' crypto has no MD5,
/// and Last.fm's signatures are MD5.
export function md5(text) {
  const bytes = new TextEncoder().encode(text);
  const words = new Uint32Array((((bytes.length + 8) >>> 6) + 1) * 16);
  for (let i = 0; i < bytes.length; i++) words[i >>> 2] |= bytes[i] << ((i % 4) * 8);
  words[bytes.length >>> 2] |= 0x80 << ((bytes.length % 4) * 8);
  words[words.length - 2] = bytes.length * 8;
  words[words.length - 1] = Math.floor((bytes.length * 8) / 0x100000000);
  const K = new Uint32Array(64);
  for (let i = 0; i < 64; i++) K[i] = Math.floor(Math.abs(Math.sin(i + 1)) * 0x100000000);
  const S = [7, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];
  let a0 = 0x67452301, b0 = 0xefcdab89, c0 = 0x98badcfe, d0 = 0x10325476;
  for (let block = 0; block < words.length; block += 16) {
    let a = a0, b = b0, c = c0, d = d0;
    for (let i = 0; i < 64; i++) {
      let f, g;
      if (i < 16) { f = (b & c) | (~b & d); g = i; }
      else if (i < 32) { f = (d & b) | (~d & c); g = (5 * i + 1) % 16; }
      else if (i < 48) { f = b ^ c ^ d; g = (3 * i + 5) % 16; }
      else { f = c ^ (b | ~d); g = (7 * i) % 16; }
      const shift = S[(i >>> 4) * 4 + (i % 4)];
      const sum = (a + f + K[i] + words[block + g]) >>> 0;
      a = d; d = c; c = b;
      b = (b + ((sum << shift) | (sum >>> (32 - shift)))) >>> 0;
    }
    a0 = (a0 + a) >>> 0; b0 = (b0 + b) >>> 0; c0 = (c0 + c) >>> 0; d0 = (d0 + d) >>> 0;
  }
  return [a0, b0, c0, d0]
    .map((v) => [v & 255, (v >>> 8) & 255, (v >>> 16) & 255, (v >>> 24) & 255].map((x) => x.toString(16).padStart(2, "0")).join(""))
    .join("");
}
