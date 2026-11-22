# Compliance

ZBQ is a fork of QBZ (MIT, by blitzkriegfc and contributors; see `LICENSE`, `NOTICE.md`
and `licenses/`). QBZ was shut down by its author after Qobuz objected. ZBQ keeps
everything that is legal to copy and run, and removes everything that is not.

The bar is the law first. Qobuz's terms of use are followed where possible; where a
feature can only work by breaking them, it may stay only if it breaks no law.

## Removed, and why

Getting around a technical measure that controls access to copyrighted works is
illegal (DMCA §1201 in the US, Article 6 of the EU InfoSoc Directive), and so is
distributing code that does it. Merely disabling such code is not enough, because
the source would still be distributed, so all of the following is **deleted**:

| Removed | What it did |
|---|---|
| `qbz-qobuz/src/bundle.rs` | Extracted Qobuz's app secret from the official web player's JavaScript bundle |
| Request signing (`sign_*`, `signed_get*`, `secret()`) | Used that secret to sign API requests |
| OAuth code exchange (`exchange_oauth_code`, `login_with_oauth_code`) | Finished sign-in with the web player's private key, also scraped from the bundle |
| Stream-URL acquisition (`get_stream_url*`, `get_file_url`, `ensure_cmaf_session`) | Obtained protected stream URLs with the signature |
| `qbz-cmaf`, `qbz-qobuz/src/cmaf.rs` | Decrypted Qobuz's encrypted CMAF stream segments |
| `qbz-offline-cache` | Downloaded Qobuz streams to an encrypted vault and decrypted them for offline play |
| Signed purchase downloads (`purchases.rs`, `download_purchase_track*`) | Fetched purchased files through the signed file-URL call |
| Re-serving Qobuz audio to cast devices (`open_external_stream`, `fetch_for_external_stream`) | Decrypted Qobuz audio and served it to Chromecast/DLNA |
| `qconnect-*` crates and their UI | Qobuz Connect; its authentication rode the same signed calls |

The release binary contains none of the endpoint strings this code needed (check with
`grep -a -c track/getFileUrl crates/target/release/zbq` and so on; `request_sig`
appears only in the log redactor, which scrubs such values from logs).

## How Qobuz works now

* **Sign-in** happens on Qobuz's own sign-in page: in a normal Firefox window when
  Firefox hosts the official player (nothing automates that page), otherwise inside the
  app (`qml/engine/QobuzEngine.qml`). The app then reads the user's own session token
  from the official player. It never handles a Qobuz secret or key.
* **Catalog, library and account calls** (`qbz-qobuz`) send only the official web
  player's public app ID and the user's own token, unsigned, as the web player does.
* **Playback** is driven through Qobuz's official web player, running in a hidden
  Firefox when Firefox is installed (`firefox_engine.rs`, next section), otherwise in
  Qt WebEngine. `engine/qobuz_engine_bridge.js` calls the player's own action creators
  (`replacePlayqueue`, `play`, `pause`, `seek`, `setVolume`, `setMaxAudioFormat`)
  exactly as its play buttons do. Qobuz's code does all fetching, signing and
  decryption. The app never sees a stream URL or a key, and never plays Qobuz audio
  itself: the browser does.
* **Purchases** are listed in the app. Download them from qobuz.com, then add the
  folder to the library.

## Bit-perfect Qobuz playback: the official player in Firefox (2026-11-16)

ZBQ's aim is bit-perfect audio. Chromium (Qt WebEngine) cannot give it: measured
on Linux, it resamples every stream to one fixed output rate and fades each start in
over 5 ms. Firefox can: it opens its audio output at each file's own sample rate and
passes the samples through unchanged. So when Firefox is installed:

* Qobuz's official player runs in a hidden Firefox with ZBQ's own profile, driven over
  WebDriver BiDi (`firefox_engine.rs`, `bidi.rs`) with the same bridge script as above.
* Before a track plays, ZBQ sets PipeWire to the track's sample rate
  (`clock.force-rate`, as its native PipeWire backend does for its own streams).
* `engine/qobuz_engine_shadow.js` keeps the output continuous: Qobuz's player gives
  each track its own media source, so the browser would close its output at every track
  change (a gap, and the last moment of the track lost). The script mutes the page's own
  audio element and hands the media data the page has just given it to a second player
  in the same page, which plays the queued tracks back to back. A run of tracks ends with
  a quarter second of digital silence, so the browser's final cut only takes silence.

Measured on real Qobuz tracks (24/96, 24/192, 16/44.1), against the tracks' own FLAC
checked with the MD5 in their stream info: every sample identical, start to end, and a
gapless transition joined with no sample missing or added.

What ZBQ never does: decrypt anything, touch a key, fetch a stream, play Qobuz audio in
its own engine, or write Qobuz audio anywhere. The media data never leaves the browser.

Legal status, plainly: no protection is bypassed. Qobuz's player decrypts with Qobuz's
authority, for the subscriber's own playback, and the browser plays the result; ZBQ's
script only routes it to a second player inside that browser. `ZBQ_QOBUZ_AUDIO=web`
uses Qt WebEngine and the official player's own (not bit-perfect) audio instead.

## What this costs

* No offline Qobuz downloads, no Qobuz Connect, no casting of Qobuz tracks.
* Bit-perfect Qobuz needs Firefox: the installed one, or Firefox ESR downloaded from
  Mozilla (checked against the SHA-512 Mozilla publishes) when none is installed and the
  user agrees to the download. With
  `ZBQ_QOBUZ_AUDIO=web`, Qobuz audio is not bit-perfect.
* Pause is Firefox's own: resuming skips the few milliseconds of audio Firefox had
  already handed to PipeWire. (Pausing the stream inside PipeWire kept every sample, but
  it preceded each reset of a USB headset in testing, so it is not used.)
* The volume control sets the output device's own volume; the stream stays at 100%.
* PipeWire is never set to a rate the output device lacks: a 44.1 kHz track on a
  48/96 kHz-only device, say, plays at the nearest rate the device has (96 kHz there),
  resampled by PipeWire, and ZBQ shows it as not bit-perfect.
* Gapless works between tracks of the same format; a change of sample rate has a short
  gap while the output switches (as with the native engine).
* While Qobuz plays, PipeWire runs at the track's rate for every app, as with the native
  PipeWire backend. "Lock output device" in the audio settings opts out (PipeWire then
  resamples).

## Residual risk

Using Qobuz's service through a third-party app, and automating its official web
player, is likely against Qobuz's terms of use. That is a contract matter between the
user and Qobuz (the usual consequence is account termination), not circumvention: no
protection is bypassed, and Qobuz's own code does everything protected. The app's
name, ZBQ, contains neither "Qobuz" nor "QBZ" (Qobuz's trademark; QBZ's author
disowned forks), and its identifiers (app ID `lol.zbq.ZBQ`, `zbq://` links, data in
`zbq` folders, keyring service `zbq`) never collide with a QBZ install. "Qobuz" may
only be used to say what service the app works with.

## Do not reintroduce

Anything that extracts or uses a Qobuz secret or private key, signs requests with one,
fetches stream URLs, decrypts Qobuz media, or saves Qobuz streams (to disk, a cache or
a file of any kind), or passes Qobuz media out of the browser. The in-page stream
(`qobuz_engine_shadow.js`) stays within these lines: playback only, inside the browser,
nothing decrypted, fetched or stored by ZBQ.

ZBQ's published history starts from the cleaned tree: none of the removed code is in it.
