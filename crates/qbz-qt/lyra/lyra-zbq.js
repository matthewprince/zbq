// ZBQ host for Lyra (lyra.js ships verbatim; this file only wraps it).
//
// The app drives the page with runJavaScript calls on window.zbq: init (once),
// clock (the playback position, a few times a second), load (a track's lyrics
// and what is known about it), cover, compact, start/stop, refetchDone. The
// page answers through console messages the view reads ("zbq:seek:<ms>" when a
// line is clicked, "zbq:refetch:1" for Lyra's "Wrong lyrics?" pill), so no web
// channel is needed.
//
// Lyra smooths the clock itself (it eases toward getPos and snaps on a jump of
// more than 900 ms), so getPos only has to extrapolate from the last position
// the app sent.
(function () {
  "use strict";
  if (!window.Lyra || !window.Lyra.create) return;

  var clock = { pos: 0, playing: false, at: performance.now() };
  var r = null;
  var baseOffset = 110;
  var analysisFor = null;
  var analysisCache = {};
  // "Wrong lyrics?": the pill waits on the app (which asks Qobuzify for another
  // version and, when there is one, sends it as a new document) for the
  // message it shows. Capped, so a lost answer can't leave it on "Fetching…".
  var refetchWaiting = null;
  var REFETCH_FAILED = "Couldn't check for another version";

  function getPos() {
    return clock.playing ? clock.pos + (performance.now() - clock.at) : clock.pos;
  }
  function post(kind, value) {
    try { console.info("zbq:" + kind + ":" + value); } catch (e) {}
  }

  // Zero-width characters some providers leave in lyric text: invisible, but they
  // break word grouping and make blank-looking lines.
  var ZW = /[​-‍⁠﻿]/g;
  function noZw(s) { return String(s == null ? "" : s).replace(ZW, ""); }

  // What Lyra renders right: its parser takes the Qobuzify Syllable/Line JSON,
  // LRC and plain text. A Qobuzify "Static" sheet keeps its text in Lines, which
  // the JSON adapter does not read, so it becomes plain text first.
  function prepare(src) {
    if (src == null) return null;
    if (typeof src === "string") {
      var s = src.trim();
      if (s[0] !== "{") return noZw(src);
      try { src = JSON.parse(s); } catch (e) { return noZw(src); }
    }
    if (src.Type === "Static" && !(src.Content && src.Content.length)) {
      return (src.Lines || []).map(function (l) { return noZw(l && l.Text).replace(/\s+/g, " ").trim(); })
        .filter(Boolean).join("\n");
    }
    return JSON.parse(JSON.stringify(src), function (k, v) {
      return k === "Text" && typeof v === "string" ? noZw(v) : v;
    });
  }

  // Audio-reactive background data from the Qobuzify API. Decorative and best
  // effort: never holds up the lyrics, a miss leaves the background calm.
  function fetchAnalysis(meta) {
    if (!r || !meta || !meta.name || !meta.artist) return;
    var key = meta.name + "|" + meta.artist;
    analysisFor = key;
    if (Object.prototype.hasOwnProperty.call(analysisCache, key)) { r.setAnalysis(analysisCache[key]); return; }
    r.setAnalysis(null);
    var u = "https://api.qobuzify.app/v2/analysis?qz=1&fields=energy,segments,beats" +
      "&name=" + encodeURIComponent(meta.name) + "&artist=" + encodeURIComponent(meta.artist);
    if (meta.album) u += "&album=" + encodeURIComponent(meta.album);
    if (meta.durationMs) u += "&durationMs=" + Math.round(meta.durationMs);
    if (meta.isrc) u += "&isrc=" + encodeURIComponent(meta.isrc);
    fetch(u).then(function (res) { return res.json(); }).then(function (j) {
      var a = j && j.ok && j.analysis && j.analysis.available ? j.analysis : null;
      analysisCache[key] = a;
      if (analysisFor === key && r) r.setAnalysis(a);
    }).catch(function () { analysisCache[key] = null; });
  }

  window.zbq = {
    init: function (opts) {
      if (r) return;
      opts = opts || {};
      document.documentElement.classList.toggle("zbq-compact", !!opts.compact);
      if (+opts.scale > 0) document.documentElement.style.setProperty("--zbq-scale", String(+opts.scale));
      r = window.Lyra.create({
        mount: document.getElementById("lyrics"),
        getPos: getPos,
        isPlaying: function () { return clock.playing; },
        // Lyra lights a line timingOffsetMs early and hands over its nominal start:
        // seek that much earlier, so the clicked line lights as the seek lands.
        onSeek: function (ms) {
          var off = r && r.settings && typeof r.settings.timingOffsetMs === "number" ? r.settings.timingOffsetMs : baseOffset;
          post("seek", Math.max(0, Math.round((+ms || 0) - off)));
        },
        onRefetch: function () {
          return new Promise(function (resolve) {
            if (refetchWaiting) refetchWaiting(REFETCH_FAILED);
            refetchWaiting = resolve;
            post("refetch", 1);
            setTimeout(function () {
              if (refetchWaiting === resolve) { refetchWaiting = null; resolve(REFETCH_FAILED); }
            }, 20000);
          });
        },
        settings: { background: opts.background !== false, closeButton: false, credits: true },
      });
      if (r.settings && typeof r.settings.timingOffsetMs === "number") baseOffset = r.settings.timingOffsetMs;
      post("ready", 1);
    },
    clock: function (pos, playing) {
      clock.pos = +pos || 0;
      clock.playing = !!playing;
      clock.at = performance.now();
    },
    load: function (src, meta) {
      if (!r) return false;
      // load() appends to what is mounted: clear first.
      r.load({ lines: [] });
      var ok = r.load(prepare(src));
      r.start();
      fetchAnalysis(meta);
      return ok;
    },
    status: function (msg) { if (r) { r.load({ lines: [] }); r.status(msg || null); } },
    cover: function (url) { if (r && url) r.setCover(url); },
    compact: function (on) {
      document.documentElement.classList.toggle("zbq-compact", !!on);
      if (r) r.remeasure();
    },
    // The lyrics text-size setting, as a factor on Lyra's type size.
    scale: function (factor) {
      var f = +factor > 0 ? +factor : 1;
      document.documentElement.style.setProperty("--zbq-scale", String(f));
      if (r) r.remeasure();
    },
    offset: function (ms) { if (r) r.setOffset(baseOffset + (+ms || 0)); },
    start: function () { if (r) r.start(); },
    stop: function () { if (r) r.stop(); },
    stats: function () { return r ? JSON.stringify(r.stats()) : "{}"; },
    refetchDone: function (message) {
      var done = refetchWaiting;
      refetchWaiting = null;
      if (done) done(String(message || REFETCH_FAILED));
    },
  };
})();
