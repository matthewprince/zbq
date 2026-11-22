// ZBQ: one continuous output stream for Qobuz's official player in Firefox
// (firefox_engine.rs). Injected before the page's own scripts.
//
// Qobuz's player gives every track its own MediaSource, so the browser closes and
// reopens its audio output at each track change: a gap, and the end of the track
// (the last buffer still on its way to the sound card) is lost. Here the page's own
// <audio> element is muted, and the plain FLAC-in-MP4 the page hands to it is
// passed, inside this page, to a player of ours: one MediaSource per run of tracks
// at the same sample rate, fed back to back in "sequence" mode, so the output never
// closes between them. A run ends with a quarter second of digital silence, so the
// browser's final cut only ever takes silence. The audio never leaves the browser;
// nothing here fetches, decrypts or stores anything.
//
// The app drives it through window.__zbqShadow (end of this file): which tracks to
// take (reset, queueNext), transport, volume, and the PipeWire rate handshake (a run
// starts only after the app has set the output to its sample rate: needRate, ack).
(function () {
  if (window.__zbqShadow) return;
  var SBp = SourceBuffer.prototype, MSp = MediaSource.prototype, MEp = HTMLMediaElement.prototype;
  var nAppend = SBp.appendBuffer, nRemove = SBp.remove, nAddSB = MSp.addSourceBuffer, nEOS = MSp.endOfStream, nPlay = MEp.play;
  var mutedD = Object.getOwnPropertyDescriptor(MEp, "muted");
  var tsoD = Object.getOwnPropertyDescriptor(SBp, "timestampOffset");
  var PREBUFFER = 2; // seconds buffered before a run starts
  var AHEAD = 15;    // seconds a run keeps buffered ahead of its playhead (the rest waits here)
  var FINAL = 3;     // a run with nothing more coming is closed this close to its end
  var PAD = 0.06;    // seconds of digital silence that close a run (more than one cycle of the
                     // sound server, which is all the browser's final cut can take)

  var sh = { gen: 0, expect: [], tracks: [], runs: [], paused: false, volume: 1, ended: false, startSec: 0, acked: 0, log: [] };

  function L(m) { sh.log.push(m); if (sh.log.length > 200) sh.log.shift(); }
  function copy(buf) {
    if (buf instanceof ArrayBuffer) return new Uint8Array(buf.slice(0));
    if (ArrayBuffer.isView(buf)) return new Uint8Array(buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength));
    return null;
  }
  function endOf(r) { return r && r.length ? r.end(r.length - 1) : 0; }
  function type4(u8, i) { return String.fromCharCode(u8[i], u8[i + 1], u8[i + 2], u8[i + 3]); }
  function isInit(u8) { var t = type4(u8, 4); return t === "ftyp" || t === "moov"; }
  function hasMoof(u8) {
    var dv = new DataView(u8.buffer, u8.byteOffset, u8.byteLength), i = 0;
    while (i + 8 <= u8.length) {
      var size = dv.getUint32(i);
      if (type4(u8, i + 4) === "moof") return true;
      if (size === 1 && i + 16 <= u8.length) size = dv.getUint32(i + 12) + dv.getUint32(i + 8) * 4294967296;
      if (size < 8) return false;
      i += size;
    }
    return false;
  }
  function findType(u8, t) {
    for (var i = 4; i + 4 <= u8.length; i++) if (type4(u8, i) === t) return i;
    return -1;
  }
  // track_ID, timescale and STREAMINFO from an init segment
  function parseInit(u8) {
    var dv = new DataView(u8.buffer, u8.byteOffset, u8.byteLength), info = {}, p;
    p = findType(u8, "tkhd"); if (p > 0) info.trackId = dv.getUint32(p + 8 + (u8[p + 4] === 1 ? 16 : 8));
    p = findType(u8, "mdhd"); if (p > 0) info.timescale = dv.getUint32(p + 8 + (u8[p + 4] === 1 ? 16 : 8));
    p = findType(u8, "dfLa");
    if (p > 0) {
      var s = p + 12, hi = dv.getUint32(s + 10);
      info.maxBlock = dv.getUint16(s + 2);
      info.rate = hi >>> 12; info.channels = ((hi >>> 9) & 7) + 1; info.bps = ((hi >>> 4) & 31) + 1;
      info.total = (hi & 15) * 4294967296 + dv.getUint32(s + 14);
    }
    return info;
  }
  function sameFormat(a, b) { return a && b && a.rate === b.rate && a.channels === b.channels && a.bps === b.bps; }

  // ---- digital silence in the run's own format, as one FLAC-in-MP4 fragment ----
  function crc8(a) { var c = 0; for (var i = 0; i < a.length; i++) { c ^= a[i]; for (var k = 0; k < 8; k++) c = c & 0x80 ? ((c << 1) ^ 0x07) & 255 : (c << 1) & 255; } return c; }
  function crc16(a) { var c = 0; for (var i = 0; i < a.length; i++) { c ^= a[i] << 8; for (var k = 0; k < 8; k++) c = c & 0x8000 ? ((c << 1) ^ 0x8005) & 0xFFFF : (c << 1) & 0xFFFF; } return c; }
  var BS = { 192: 1, 576: 2, 1152: 3, 2304: 4, 4608: 5, 256: 8, 512: 9, 1024: 10, 2048: 11, 4096: 12, 8192: 13, 16384: 14, 32768: 15 };
  function silentFrame(bs, ch, bps, num) {
    var code = BS[bs] || (bs <= 256 ? 6 : 7), ss = bps === 16 ? 4 : 6;
    var f = [0xFF, 0xF8, code << 4, ((ch - 1) << 4) | (ss << 1), num & 0x7F];
    if (code === 6) f.push((bs - 1) & 255);
    if (code === 7) f.push(((bs - 1) >> 8) & 255, (bs - 1) & 255);
    f.push(crc8(f));
    for (var c = 0; c < ch; c++) { f.push(0); for (var b = 0; b < bps / 8; b++) f.push(0); } // CONSTANT 0 subframes
    var x = crc16(f); f.push(x >> 8, x & 255);
    return f;
  }
  function u32(n) { return [(n >>> 24) & 255, (n >>> 16) & 255, (n >>> 8) & 255, n & 255]; }
  function box(t, parts) {
    var n = 8; parts.forEach(function (p) { n += p.length; });
    var out = new Uint8Array(n), o = 8;
    out.set(u32(n), 0); out.set([t.charCodeAt(0), t.charCodeAt(1), t.charCodeAt(2), t.charCodeAt(3)], 4);
    parts.forEach(function (p) { out.set(p, o); o += p.length; });
    return out;
  }
  function silence(info) {
    if (!info || !info.maxBlock || !info.timescale || (info.bps !== 16 && info.bps !== 24)) return null;
    var count = Math.ceil(PAD * info.rate / info.maxBlock), frames = [], entries = [], i;
    var dur = Math.round(info.maxBlock * info.timescale / info.rate);
    for (i = 0; i < count; i++) {
      var fr = silentFrame(info.maxBlock, info.channels, info.bps, i);
      frames.push(fr); entries = entries.concat(u32(dur), u32(fr.length));
    }
    var moofLen = 8 + 16 + 8 + 16 + 20 + (20 + 8 * count);
    var trun = box("trun", [[0, 0, 3, 1], u32(count), u32(moofLen + 8), entries]);
    var traf = box("traf", [box("tfhd", [[0, 2, 0, 0], u32(info.trackId || 1)]), box("tfdt", [[1, 0, 0, 0], u32(0), u32(0)]), trun]);
    var moof = box("moof", [box("mfhd", [[0, 0, 0, 0], u32(1)]), traf]);
    var mdat = box("mdat", [[].concat.apply([], frames)]);
    var out = new Uint8Array(moof.length + mdat.length);
    out.set(moof, 0); out.set(mdat, moof.length);
    return out;
  }

  // ---- runs: one MediaSource + element per stretch of same-format tracks ----
  function newRun(t) {
    var r = { info: t.info, mime: t.mime, audio: document.createElement("audio"), ms: new MediaSource(), sb: null, q: [],
              tracks: [t], started: false, ended: false, finalized: false, waiting: false, resetTo: null };
    r.ms.addEventListener("sourceopen", function () {
      try {
        r.sb = nAddSB.call(r.ms, r.mime);
        r.sb.mode = "sequence";
        r.sb.addEventListener("updateend", function () { pump(r); });
      } catch (e) { L("run buffer failed: " + e); }
      pump(r);
    }, { once: true });
    r.audio.addEventListener("ended", function () { r.ended = true; L("run at " + r.info.rate + " Hz played to its end"); advance(); });
    r.audio.addEventListener("waiting", function () { r.waiting = true; });
    r.audio.addEventListener("playing", function () { r.waiting = false; });
    r.audio.addEventListener("error", function () { L("run media error: " + (r.audio.error && r.audio.error.message)); });
    r.audio.__zbqRun = true;
    r.audio.volume = sh.volume;
    r.audio.src = URL.createObjectURL(r.ms);
    sh.runs.push(r);
    sh.ended = false;
    L("new run at " + t.info.rate + " Hz " + t.info.bps + "-bit for track " + t.tid);
    return r;
  }
  function destroyRun(r) {
    r.ended = true; r.q = [];
    try { r.audio.pause(); } catch (e) {}
    try { var src = r.audio.src; r.audio.removeAttribute("src"); r.audio.load(); URL.revokeObjectURL(src); } catch (e) {}
  }
  function advance() {
    while (sh.runs.length && sh.runs[0].ended) destroyRun(sh.runs.shift());
    if (!sh.runs.length) { sh.ended = true; L("everything queued has played"); }
  }
  function assign(t) {
    var last = sh.runs[sh.runs.length - 1];
    if (last && !last.ended && !last.finalized && sameFormat(last.info, t.info)) {
      last.tracks.push(t); t.run = last;
      L("track " + t.tid + " joins the running stream (gapless)");
    } else {
      t.run = newRun(t);
    }
  }
  function evict(r) {
    var ct = r.audio.currentTime, b = r.sb.buffered;
    if (b.length && ct - 2 > b.start(0)) { try { nRemove.call(r.sb, b.start(0), ct - 2); } catch (e) {} }
  }
  function pump(r) {
    if (!r.sb || r.sb.updating || r.ended) return;
    while (r.q.length) {
      var it = r.q[0];
      try {
        if (it.k === "cut") { // a restarted track: drop what it put in, continue from where it began
          r.q.shift();
          r.resetTo = it.at;
          var e = endOf(r.sb.buffered);
          if (e > it.at) { nRemove.call(r.sb, it.at, e + 1); return; }
          continue;
        }
        if (it.k === "a") {
          if (endOf(r.sb.buffered) - r.audio.currentTime > AHEAD + (r.started ? 0 : PREBUFFER)) return;
          var at = r.resetTo !== null ? r.resetTo : endOf(r.sb.buffered);
          if (r.resetTo !== null) { tsoD.set.call(r.sb, r.resetTo); r.resetTo = null; }
          if (it.t.sessStart === null) it.t.sessStart = at;
          nAppend.call(r.sb, it.d);
          r.q.shift();
          return;
        }
        if (it.k === "pad") {
          r.q.shift();
          r.realEnd = endOf(r.sb.buffered); // where the music stops and the silence begins
          var pad = silence(it.info);
          if (pad) { nAppend.call(r.sb, pad); return; }
          L("no silence pad for this format");
          continue;
        }
        if (it.k === "eos") {
          r.q.shift();
          if (r.ms.readyState === "open") nEOS.call(r.ms);
          continue;
        }
        r.q.shift();
      } catch (e) {
        if (e && e.name === "QuotaExceededError") { evict(r); return; } // the tick retries
        L("run " + it.k + " failed: " + (e && e.name));
        r.q.shift();
      }
    }
  }
  // Close a run once everything it will get is in: at once when a later run exists
  // (a format change), otherwise once nothing more can join it and it nears its end.
  function finalizeIfDone() {
    for (var n = 0; n < sh.runs.length; n++) {
      var r = sh.runs[n];
      if (r.finalized || r.ended || !r.sb || r.q.length) continue;
      var last = r.tracks[r.tracks.length - 1];
      if (!last || !last.eos) continue;
      if (n === sh.runs.length - 1) {
        var i = sh.expect.indexOf(last.tid), pending = false;
        if (i >= 0 && i < sh.expect.length - 1) {
          pending = true; // the successor's first bytes may still be on their way
          for (var k = 0; k < sh.tracks.length; k++) if (sh.tracks[k].tid === sh.expect[i + 1] && sh.tracks[k].info) pending = false;
        }
        if (pending || endOf(r.sb.buffered) - r.audio.currentTime > FINAL) continue;
      }
      r.finalized = true;
      r.q.push({ k: "pad", info: last.info }, { k: "eos" });
      pump(r);
    }
  }
  function needRate() {
    var r = sh.runs[0];
    return r && !r.started && sh.acked !== r.info.rate ? r.info.rate : 0;
  }
  function startFront() {
    var r = sh.runs[0];
    if (!r || r.started || !r.sb || sh.paused || sh.acked !== r.info.rate) return;
    var first = r.tracks[0];
    if (!first || first.sessStart === null || performance.now() - first.created < 1200) return;
    var want = first.sessStart + Math.max(0, sh.startSec - first.dataStart); // where in the run to begin
    var b = r.sb.buffered, have = endOf(b);
    var complete = r.finalized && !r.q.length;
    if (!(b.length && b.start(0) <= want + 0.001 && have > want)) return; // the start position is not here yet
    if (have - want < PREBUFFER && !complete) return;
    if (want > first.sessStart) r.audio.currentTime = want;
    r.started = true;
    sh.startSec = 0;
    nPlay.call(r.audio).then(function () { L("playing " + first.tid + " at " + r.info.rate + " Hz"); },
      function (e) { r.started = false; L("play refused: " + e); });
  }
  // A run followed by another (a format change) is let go as soon as its last real
  // sample has played: waiting for the browser's own end (after the silence pad and
  // its drain) only lengthens the gap. The clock trails what the sound card has
  // already been given, and a margin keeps the cut inside the silence.
  function releaseFinishedFront() {
    var r = sh.runs[0];
    if (sh.runs.length < 2 || !r || !r.started || r.ended || r.realEnd === undefined) return;
    if (r.audio.currentTime >= r.realEnd + 0.05) {
      r.ended = true;
      L("run at " + r.info.rate + " Hz played to its last sample");
      advance();
    }
  }
  setInterval(function () {
    for (var i = 0; i < sh.runs.length; i++) pump(sh.runs[i]);
    finalizeIfDone();
    releaseFinishedFront();
    startFront();
  }, 50);

  // The track the official player is loading, from its own state (the ZBQ bridge).
  function pageTrackId() {
    try { return window.__zbq ? String(JSON.parse(window.__zbq.state()).trackId || "") : ""; } catch (e) { return ""; }
  }
  function dropTrack(t) {
    t.dropped = true;
    var r = t.run;
    if (!r) return;
    if (r.tracks.length === 1) {
      destroyRun(r);
      sh.runs = sh.runs.filter(function (x) { return x !== r; });
    } else if (r.tracks[r.tracks.length - 1] === t) {
      r.tracks.pop();
      r.q = r.q.filter(function (it) { return it.t !== t; });
      if (t.sessStart !== null) r.q.push({ k: "cut", at: t.sessStart });
    }
  }

  // ---- hooks on the page's own media objects ----
  MEp.play = function () {
    if (!this.__zbqRun) { this.__zbqPage = true; mutedD.set.call(this, true); }
    return nPlay.apply(this, arguments);
  };
  Object.defineProperty(MEp, "muted", {
    configurable: true, enumerable: mutedD.enumerable,
    get: function () { return this.__zbqPage ? !!this.__zbqWantMuted : mutedD.get.call(this); },
    set: function (v) {
      if (this.__zbqRun) return mutedD.set.call(this, v);
      this.__zbqPage = true; this.__zbqWantMuted = !!v; mutedD.set.call(this, true);
    }
  });
  MSp.addSourceBuffer = function (mime) {
    var sb = nAddSB.apply(this, arguments);
    try {
      if (/^audio\//i.test(String(mime)) && sh.expect.length) {
        var tid = pageTrackId(), prev = null, i;
        for (i = sh.tracks.length - 1; i >= 0; i--) if (sh.tracks[i].tid === tid && !sh.tracks[i].dropped) { prev = sh.tracks[i]; break; }
        if (sh.expect.indexOf(tid) < 0) L("source buffer for track " + tid + ": not queued, ignored");
        else if (prev && prev.eos) L("track " + tid + " reloaded by the page after it was complete: ignored");
        else if (prev && prev.run && prev.run.started && prev.sessStart !== null && prev.run.audio.currentTime - prev.sessStart > 5) L("track " + tid + " restarted by the page while playing: ignored");
        else {
          if (prev) { dropTrack(prev); L("track " + tid + ": the page's false start dropped"); }
          var t = { tid: tid, sb: sb, ms: this, mime: String(mime), gen: sh.gen, appends: 0, eos: false, dropped: false,
                    hasMedia: false, info: null, run: null, dataStart: 0, sessStart: null, created: performance.now() };
          sb.__zbqT = t;
          sh.tracks.push(t);
        }
      }
    } catch (e) { L("addSourceBuffer hook: " + e); }
    return sb;
  };
  SBp.appendBuffer = function (buf) {
    var res = nAppend.apply(this, arguments); // the page's own append first; a refused one is not copied
    try {
      var t = this.__zbqT;
      if (t && t.gen === sh.gen && !t.dropped) {
        var u8 = copy(buf);
        if (u8) {
          if (!t.info) {
            if (!isInit(u8)) { L("track " + t.tid + ": media before its init segment, ignored"); return res; }
            t.info = parseInit(u8);
            if (!t.info.rate) { L("track " + t.tid + ": no FLAC stream info, ignored"); t.dropped = true; return res; }
            assign(t);
          }
          if (hasMoof(u8)) t.hasMedia = true;
          t.appends++;
          t.run.q.push({ k: "a", t: t, d: u8 });
          pump(t.run);
        }
      }
    } catch (e) { L("append hook: " + e); }
    return res;
  };
  Object.defineProperty(SBp, "timestampOffset", {
    configurable: true, enumerable: tsoD.enumerable, get: tsoD.get,
    set: function (v) {
      try {
        var t = this.__zbqT;
        if (t && t.gen === sh.gen && !t.dropped) {
          if (!t.hasMedia) t.dataStart = Number(v) || 0;
          else if (t.run && t.run.tracks[t.run.tracks.length - 1] === t) {
            // The page moved inside this track (a seek): what came before goes.
            L("track " + t.tid + " restarts at " + Number(v).toFixed(3) + " s");
            t.run.q = t.run.q.filter(function (it) { return it.t !== t; });
            if (t.sessStart !== null) t.run.q.push({ k: "cut", at: t.sessStart });
            t.sessStart = null; t.dataStart = Number(v) || 0; t.hasMedia = false; t.eos = false;
          }
        }
      } catch (e) {}
      return tsoD.set.call(this, v);
    }
  });
  MSp.endOfStream = function () {
    try {
      for (var i = 0; i < sh.tracks.length; i++) {
        var t = sh.tracks[i];
        if (t.ms === this && t.gen === sh.gen && !t.eos && !t.dropped && t.info) { t.eos = true; L("track " + t.tid + " fully received"); }
      }
    } catch (e) {}
    return nEOS.apply(this, arguments);
  };

  function current() {
    var r = sh.runs[0];
    if (!r) return null;
    var now = r.audio.currentTime, cur = r.tracks[0];
    for (var i = 0; i < r.tracks.length; i++) {
      var t = r.tracks[i];
      if (t.sessStart !== null && t.sessStart <= now + 0.0005) cur = t;
    }
    var time = r.started && cur.sessStart !== null ? cur.dataStart + (now - cur.sessStart) : Math.max(sh.startSec, cur.dataStart);
    return { run: r, t: cur, time: time };
  }

  // The stretch of the current track the stream holds around the play position,
  // in track seconds [from, to]; null when nothing is held.
  function held(c) {
    if (!c || !c.run.sb || c.t.sessStart === null) return null;
    var r = c.run, b = r.sb.buffered, now = r.audio.currentTime;
    for (var i = 0; i < b.length; i++) {
      if (b.start(i) > now + 0.0005 || b.end(i) <= now) continue;
      var end = b.end(i), k = r.tracks.indexOf(c.t), next = k >= 0 ? r.tracks[k + 1] : null;
      if (next && next.sessStart !== null) end = Math.min(end, next.sessStart);
      else if (r.realEnd !== undefined) end = Math.min(end, r.realEnd);
      var from = Math.max(b.start(i), c.t.sessStart);
      return [c.t.dataStart + (from - c.t.sessStart), c.t.dataStart + (end - c.t.sessStart)];
    }
    return null;
  }

  Object.defineProperty(window, "__zbqShadow", {
    enumerable: false,
    value: {
      // Take `tid` (a string) from the official player, `startMs` into it; anything
      // held before is dropped. `gen` comes back in state() so the app can tell
      // answers about this start from older ones.
      reset: function (tid, startMs, gen) {
        sh.runs.forEach(destroyRun); sh.runs = [];
        sh.tracks.forEach(function (t) { t.dropped = true; }); sh.tracks = [];
        sh.gen = gen | 0; sh.expect = [String(tid)]; sh.startSec = Math.max(0, (Number(startMs) || 0) / 1000);
        sh.ended = false; sh.paused = false; sh.acked = 0;
        L("start " + tid + " at " + sh.startSec.toFixed(3) + " s");
        return true;
      },
      // A seek inside what the stream holds of the current track: move there, no
      // reload. (A reload depends on the official player loading the track
      // again, and it does not reload what it already holds: dropping this copy
      // could leave the stream waiting for audio that never comes.) false when
      // that position is not held.
      seekTo: function (ms) {
        var c = current(), h = held(c), s = Math.max(0, (Number(ms) || 0) / 1000);
        if (!c || !h || !c.run.started || s < h[0] || s > h[1] - 1) return false;
        c.run.audio.currentTime = c.t.sessStart + (s - c.t.dataStart);
        sh.ended = false;
        L("seek within the stream to " + s.toFixed(3) + " s");
        return true;
      },
      // A new start position while the stream has not started yet (a seek during
      // a load): only where it begins changes.
      retarget: function (ms) {
        if (sh.runs[0] && sh.runs[0].started) return false;
        sh.startSec = Math.max(0, (Number(ms) || 0) / 1000);
        L("start moves to " + sh.startSec.toFixed(3) + " s");
        return true;
      },
      // The successor: its audio joins the running stream when the format matches.
      queueNext: function (tid) {
        if (sh.expect.indexOf(String(tid)) < 0) sh.expect.push(String(tid));
        return sh.expect.length;
      },
      stop: function () {
        sh.runs.forEach(destroyRun); sh.runs = [];
        sh.tracks.forEach(function (t) { t.dropped = true; }); sh.tracks = [];
        sh.expect = []; sh.ended = false; sh.startSec = 0; sh.acked = 0;
        return true;
      },
      // The app pauses the stream in the sound server, which keeps every sample; this
      // only holds back new runs. `hard`: also pause the element (when the sound
      // server had no stream to pause).
      pause: function (hard) {
        sh.paused = true;
        if (hard) sh.runs.forEach(function (r) { r.audio.pause(); });
        return true;
      },
      resume: function () {
        sh.paused = false;
        var r = sh.runs[0];
        if (r && r.started && r.audio.paused) nPlay.call(r.audio);
        return true;
      },
      setVolume: function (v) {
        sh.volume = Math.max(0, Math.min(1, Number(v)));
        sh.runs.forEach(function (r) { r.audio.volume = sh.volume; });
        return true;
      },
      // The app has set the output to `rate`: a run at that rate may start.
      ack: function (rate) { sh.acked = rate | 0; startFront(); return true; },
      state: function () {
        var c = current(), h = held(c);
        return JSON.stringify({
          gen: sh.gen,
          heldFromMs: h ? Math.round(h[0] * 1000) : 0,
          heldToMs: h ? Math.round(h[1] * 1000) : 0,
          track: c ? c.t.tid : "",
          timeMs: c ? Math.max(0, Math.round(c.time * 1000)) : 0,
          durationMs: c && c.t.info ? Math.round(c.t.info.total / c.t.info.rate * 1000) : 0,
          playing: !!(c && c.run.started && !sh.paused && !c.run.audio.paused && !c.run.waiting),
          started: !!(c && c.run.started),
          ended: sh.ended,
          needRate: needRate(),
          rate: c ? c.run.info.rate : 0,
          bits: c ? c.run.info.bps : 0,
          tracks: sh.tracks.filter(function (t) { return t.gen === sh.gen && !t.dropped; })
                           .map(function (t) { return { tid: t.tid, eos: t.eos, rate: t.info ? t.info.rate : 0 }; }),
          log: sh.log.splice(0)
        });
      }
    }
  });
})();
