# ZBQ API proxy

A Cloudflare Worker that holds the API keys ZBQ must not ship: Last.fm (scrobbling, stats, similar artists), Discogs (artwork, artist search) and Tidal (an app token for music links and playlist import). The app talks to it at `https://zbq.qobuzify.app`.

- Only ZBQ clients are served (User-Agent `ZBQ/...`), and only the calls ZBQ makes are forwarded.
- Nothing a user sends, Last.fm session keys included, is logged or stored.
- A service whose keys are not set answers 503, and the app falls back or hides it.
- Per-IP rate limits sit at the zone edge, before the Worker runs.

```sh
node --test test/proxy.test.mjs          # no network: upstream calls are mocked
wrangler deploy                          # then: wrangler secret put LASTFM_API_KEY, ...
```

Running your own: deploy it to a domain of yours and point the base URLs in `crates/qbz-integrations` (Last.fm, Discogs), `crates/qbz-music-link` and `crates/qbz-playlist-import` at it.
