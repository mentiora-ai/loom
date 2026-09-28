// audio_bootstrap.js — synthetic-microphone override for in-browser voice calls.
//
// voice-call-io task 03 (mic-override HALF only — Architecture §4 sections 1-3).
// Installed via `Page.addScriptToEvaluateOnNewDocument {runImmediately:true}`
// for `--audio` sessions ONLY, so it runs before the page's first
// `getUserMedia`. `__LOOM_AUDIO_NONCE__` is substituted with a fresh per-target
// nonce by `audio_bridge::render_bootstrap_script` before install.
//
// What this half does:
//   1. Mic override — `getUserMedia({audio})` resolves to a WebAudio
//      `MediaStreamAudioDestinationNode` track WE drive. A combined
//      `{audio, video}` request returns OUR synthetic audio track + the page's
//      REAL video track — the synthetic call NEVER delegates the whole request,
//      so the real microphone can never leak (plan-council FND-0022).
//   2. Provenance registry — every injected track joins a module WeakSet and
//      carries a nonce'd marker, so task 05's capture tap can exclude it.
//   3. enqueue/pending — decode caller audio and schedule it into the mic track;
//      `pending()` reports seconds still queued (the `await_playout` substrate).
//
// The public API is exposed as a NON-enumerable, NON-configurable, NON-writable
// `window.__loom_<nonce>` (plan-council FND-0019) — discoverable by design (a
// main-world override always is; AC9 non-echo holds for cooperative pages only),
// but not trivially enumerable.
//
// NOT in this half (task 05 — see the marked seam at the bottom): the capture
// tap (RTCPeerConnection ctor wrap), the ring buffer, and drain.
(function () {
  'use strict';

  var NONCE = '__LOOM_AUDIO_NONCE__';
  var API_KEY = '__loom_' + NONCE;

  // Idempotent: a belt-and-braces in-context re-eval (or a double install) is a
  // no-op once the nonce'd property exists. The exposed global is
  // `window.__loom_<nonce>` (the token is substituted at render).
  if (window.__loom___LOOM_AUDIO_NONCE__) {
    return;
  }

  // ── Section 1/2: WebAudio graph + provenance registry ────────────────────
  var ctx = null;
  var dest = null;
  var nextStart = 0;
  // AC10: an `enqueue` before any `getUserMedia` has no mic to feed → reject.
  var gumSeen = false;
  // Provenance registry (D10): injected tracks the task-05 capture tap excludes.
  var injectedTracks = (typeof WeakSet === 'function') ? new WeakSet() : null;

  function ensureGraph() {
    if (ctx) return;
    var AC = window.AudioContext || window.webkitAudioContext;
    ctx = new AC();
    dest = ctx.createMediaStreamDestination();
    // Gesture-free autoplay is enabled by the --autoplay-policy launch flag.
    try { ctx.resume(); } catch (e) {}
    nextStart = ctx.currentTime;
  }

  // Tag a track as loom-injected: WeakSet membership + a non-enumerable nonce'd
  // marker id. Task 05's capture tap consults BOTH so it never re-captures our
  // own outbound audio (AC9 non-echo).
  function tagInjectedTrack(track) {
    if (!track) return;
    try {
      if (injectedTracks) injectedTracks.add(track);
      Object.defineProperty(track, '__loomInjected', {
        value: NONCE,
        enumerable: false,
        configurable: false,
        writable: false
      });
    } catch (e) {}
  }

  // The synthetic mic: a FRESH track per getUserMedia — a clone of the
  // destination track — so the page can add/stop/clone tracks without reaching
  // into our graph. Honors the page calling `track.stop()`/`ended` (D14):
  // stopping a clone ends THAT call's microphone only. Handing out the
  // destination track itself (as before #318) meant a page that ended a call —
  // `stream.getTracks().forEach((t) => t.stop())`, which a real app must do to
  // release the microphone — ended the synthetic mic for the rest of the
  // document: the next getUserMedia returned the same, already-`ended` track,
  // and every later `say` was injected into a dead track while resolving `ok`.
  function micTracks() {
    ensureGraph();
    var source = dest.stream.getAudioTracks();
    var out = [];
    for (var i = 0; i < source.length; i++) {
      var t = source[i].clone();
      tagInjectedTrack(t);
      out.push(t);
    }
    return out;
  }

  function micStream() {
    return new MediaStream(micTracks());
  }

  function wantsAudio(c) { return !!(c && c.audio); }
  function wantsVideo(c) { return !!(c && c.video); }

  // ── Section 1: getUserMedia override (modern promise API) ────────────────
  if (navigator.mediaDevices && navigator.mediaDevices.getUserMedia) {
    var origModern = navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
    navigator.mediaDevices.getUserMedia = function (constraints) {
      try {
        var a = wantsAudio(constraints);
        var v = wantsVideo(constraints);
        if (a && !v) {
          // Audio-only voice call → pure synthetic mic.
          gumSeen = true;
          return Promise.resolve(micStream());
        }
        if (a && v) {
          // FND-0022 real-mic-leak guard: request ONLY video from the real
          // device, then compose OUR synthetic audio track with the page's
          // real video track. Never delegate the whole call.
          gumSeen = true;
          return origModern({ video: constraints.video }).then(function (real) {
            var out = new MediaStream();
            var at = micTracks();
            for (var i = 0; i < at.length; i++) out.addTrack(at[i]);
            var vt = real.getVideoTracks();
            for (var j = 0; j < vt.length; j++) out.addTrack(vt[j]);
            return out;
          });
        }
        // Pure-video (or neither) → delegate unchanged.
      } catch (e) {}
      return origModern(constraints);
    };
  }

  // Legacy callback API for older WebRTC libraries.
  var legacyGum =
    navigator.getUserMedia ||
    navigator.webkitGetUserMedia ||
    navigator.mozGetUserMedia;
  navigator.getUserMedia = function (constraints, ok, err) {
    try {
      var a = wantsAudio(constraints);
      var v = wantsVideo(constraints);
      if (a && !v) {
        gumSeen = true;
        ok(micStream());
        return;
      }
      if (a && v && legacyGum) {
        // Same leak guard on the legacy path: real video only, synthetic audio.
        gumSeen = true;
        legacyGum.call(navigator, { video: constraints.video }, function (real) {
          try {
            var out = new MediaStream();
            var at = micTracks();
            for (var i = 0; i < at.length; i++) out.addTrack(at[i]);
            var vt = real.getVideoTracks();
            for (var j = 0; j < vt.length; j++) out.addTrack(vt[j]);
            ok(out);
          } catch (e) { if (err) err(e); }
        }, err);
        return;
      }
    } catch (e) {}
    if (legacyGum) legacyGum.call(navigator, constraints, ok, err);
    else if (err) err(new Error('getUserMedia unavailable'));
  };

  // ── Section 3: enqueue + pending ─────────────────────────────────────────
  // Decode base64 audio and schedule it into the mic track. Clips play
  // back-to-back (scheduled sequentially). Resolves on `start()`, or on `ended`
  // when `awaitPlayout` (the D11 await-playout substrate). Rejects with
  // `no_microphone_request` if the page has not called getUserMedia yet (AC10).
  function enqueue(b64, awaitPlayout) {
    return new Promise(function (resolve, reject) {
      if (!gumSeen) {
        reject(new Error('no_microphone_request'));
        return;
      }
      ensureGraph();
      var bytes;
      try {
        var bin = atob(b64);
        bytes = new Uint8Array(bin.length);
        for (var i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
      } catch (e) {
        reject(e);
        return;
      }
      ctx.decodeAudioData(
        bytes.buffer,
        function (buf) {
          var src = ctx.createBufferSource();
          src.buffer = buf;
          src.connect(dest);
          var t = Math.max(ctx.currentTime, nextStart);
          src.start(t);
          nextStart = t + buf.duration;
          if (awaitPlayout) {
            src.onended = function () { resolve(buf.duration); };
          } else {
            resolve(buf.duration);
          }
        },
        function (e) { reject(e || new Error('audio_decode_failed')); }
      );
    });
  }

  // Seconds of audio still queued ahead of playout (0 when idle) — lets a driver
  // wait for an utterance to finish before listening for the reply (D11).
  function pending() {
    if (!ctx) return 0;
    return Math.max(0, nextStart - ctx.currentTime);
  }

  // Whether `track` was injected by loom (task 05 capture-tap exclusion helper).
  function isInjectedTrack(track) {
    try {
      if (injectedTracks && injectedTracks.has(track)) return true;
      return !!(track && track.__loomInjected === NONCE);
    } catch (e) {
      return false;
    }
  }

  // ── Section 4/5: capture tap (RTCPeerConnection ctor wrap) + ring + drain ──
  // (voice-call-io task 05 — Architecture §4 sections 4-5, A1/A2/A15, D9.)
  //
  // We wrap the RTCPeerConnection constructor and, on every instance, attach OUR
  // OWN 'track' listener — so we see every remote receiver regardless of whether
  // the page uses `.ontrack`, addEventListener, or only getReceivers(). For each
  // inbound AUDIO track NOT in the injected registry (A15 exclusion runs BEFORE
  // the mix), a MediaStreamTrackProcessor yields AudioData that we mix to mono at
  // the track's NATIVE rate into a bounded ring buffer. The shim drains it in
  // ≤1 MiB base64 chunks (D3), resamples host-side to 16 kHz, and WAV-muxes.

  // Hard in-page ceiling so a very long / hostile call can never exhaust renderer
  // memory before the shim's byte cap truncates it. ~180 s @ 48 kHz mono f32
  // (~34 MB). The shim's `max_bytes` is the real cap; this only bounds OOM.
  var MAX_CAPTURE_SAMPLES = 48000 * 180;

  var capturing = false;         // append to the ring only inside a capture window
  var ring = [];                 // list of Float32Array chunks (mono, native rate)
  var ringSamples = 0;           // total samples buffered
  var drainCursorChunk = 0;      // drain read position: chunk index …
  var drainCursorOffset = 0;     // … and sample offset within that chunk
  var sourceRate = 0;            // native rate of the captured track (first wins)
  var droppedFrames = 0;         // zero-filled timeline gaps ONLY (packet loss / DTX)
  var injectedLeaked = false;    // A2 tripwire: an injected track ever reached the tap
  var bufferCapHit = false;      // MAX_CAPTURE_SAMPLES reached — stop appending (truncation)
  // LIFETIME count of inbound non-injected audio tracks ever tapped (NOT reset per
  // capture window). A track that connects BEFORE startCapture must still count, so
  // an empty window is `no_samples` (silent), never a false `no_inbound_track` (I1).
  var tappedTracks = 0;
  var seenTrackIds = (typeof Set === 'function') ? new Set() : null;

  function appendSamples(mono) {
    if (!capturing || bufferCapHit || !mono || mono.length === 0) return;
    if (ringSamples + mono.length > MAX_CAPTURE_SAMPLES) {
      bufferCapHit = true;      // shim maps this to a byte_cap truncation
      return;
    }
    ring.push(mono);
    ringSamples += mono.length;
  }

  // Mix an AudioData frame to a single mono Float32Array at its native rate.
  // Averaging is done AFTER the injected-track exclusion (A15) so our own
  // outbound audio never contributes to the mix. Prefers planar copies (one per
  // channel); falls back to a single interleaved copy and de-interleaves.
  function audioDataToMono(ad) {
    var channels = ad.numberOfChannels || 1;
    var frames = ad.numberOfFrames || 0;
    if (frames === 0) return null;
    var mono = new Float32Array(frames);
    // Prefer the frame's declared format (I4); fall back to a planar attempt then
    // an interleaved attempt so an unexpected layout can't silently kill the tap.
    var interleaved = ad.format && ad.format.indexOf('planar') === -1;

    if (!interleaved) {
      try {
        var plane = new Float32Array(frames);
        for (var c = 0; c < channels; c++) {
          ad.copyTo(plane, { planeIndex: c, format: 'f32-planar' });
          for (var s = 0; s < frames; s++) mono[s] += plane[s];
        }
        if (channels > 1) for (var m = 0; m < frames; m++) mono[m] /= channels;
        return mono;
      } catch (planarErr) {
        interleaved = true; // retry as interleaved
        mono = new Float32Array(frames);
      }
    }

    // Interleaved: one copy of the whole frame, stride by channel count.
    var inter = new Float32Array(frames * channels);
    try {
      ad.copyTo(inter, { planeIndex: 0, format: 'f32' });
    } catch (interErr) {
      return null;
    }
    for (var i = 0; i < frames; i++) {
      var sum = 0;
      for (var ch = 0; ch < channels; ch++) sum += inter[i * channels + ch];
      mono[i] = (channels > 1) ? sum / channels : sum;
    }
    return mono;
  }

  // Read one inbound track to completion via Breakout Box (D9). Detects timeline
  // gaps (packet loss / DTX) by comparing each frame's timestamp to the expected
  // next timestamp and zero-filling the gap, counted in droppedFrames.
  //
  // Scope (PRD 1:1 voice call): every non-injected audio track appends into ONE
  // shared ring in arrival order and `sourceRate` is first-wins. A multi-party call
  // (2+ inbound audio tracks) would concatenate rather than mix — out of scope here.
  function tapTrack(track) {
    if (!track || track.kind !== 'audio') return;
    if (isInjectedTrack(track)) return; // A15: never tap our own outbound audio
    if (seenTrackIds) {
      if (seenTrackIds.has(track.id)) return;   // idempotent across renegotiation
      seenTrackIds.add(track.id);
    }
    if (typeof MediaStreamTrackProcessor !== 'function') return;
    var processor, reader;
    try {
      processor = new MediaStreamTrackProcessor({ track: track });
      reader = processor.readable.getReader();
    } catch (e) { return; }

    tappedTracks++;                 // I1: a real inbound track was seen (lifetime)
    // I4: release the reader's ReadableStream lock when the track ends, so a long
    // call with repeated SFU renegotiation cannot leak one reader per track. `cancel`
    // closes the stream; `releaseLock` drops the lock deterministically.
    var done = false;
    function teardown() {
      if (done) return;
      done = true;
      try { reader.cancel(); } catch (e) {}
      try { reader.releaseLock(); } catch (e) {}
    }
    try { track.addEventListener('ended', teardown); } catch (e) {}

    var expectedTs = null; // microseconds
    function pump() {
      reader.read().then(function (res) {
        if (res.done) { teardown(); return; }
        var ad = res.value;
        try {
          // Defense-in-depth (unreachable — the entry check already excludes
          // injected tracks): if our own audio ever reaches the tap, trip the A2
          // wire and stop THIS reader cleanly (don't leak it — MINOR 9).
          if (isInjectedTrack(track)) {
            injectedLeaked = true;
            try { ad.close(); } catch (e) {}
            teardown();
            return;
          }
          var rate = ad.sampleRate || 0;
          if (rate && !sourceRate) sourceRate = rate;
          // Gap detection: zero-fill only a gap larger than HALF a frame's duration
          // (not one sample) so sub-frame timestamp jitter never bumps dropped_frames
          // (A7 / MINOR 4). `frameDurUs` is the whole frame, not the per-sample period.
          var ts = (typeof ad.timestamp === 'number') ? ad.timestamp : null;
          var nframes = ad.numberOfFrames || 0;
          if (rate && ts !== null && expectedTs !== null) {
            var gapUs = ts - expectedTs;
            var frameDurUs = nframes * 1e6 / rate;
            if (gapUs > frameDurUs * 0.5) {
              var missing = Math.min(Math.round((gapUs * rate) / 1e6), rate); // cap 1 s
              if (missing > 0 && capturing && !bufferCapHit) {
                appendSamples(new Float32Array(missing)); // zeros
                droppedFrames++;
              }
            }
          }
          var mono = audioDataToMono(ad);
          appendSamples(mono);
          if (rate && ts !== null) {
            expectedTs = ts + nframes * (1e6 / rate);
          } else {
            // No usable timestamp — drop the anchor so the NEXT frame doesn't measure
            // a gap from a stale reference and double-count a zero-fill (MINOR 5).
            expectedTs = null;
          }
        } catch (e) { /* frame decode hiccup — skip */ }
        try { ad.close(); } catch (e) {}
        pump();
      }).catch(function () { teardown(); /* stream ended / errored — stop pumping */ });
    }
    pump();
  }

  // Wrap the RTCPeerConnection constructor by EXTENDING the native class (not a
  // plain function): `class extends` preserves `instanceof`, the prototype chain,
  // static methods, AND `class Page extends RTCPeerConnection` subclassing (via
  // new.target) — a plain-function wrapper silently breaks the last (I2). We attach
  // our OWN 'track' listener to each instance, never touching the page's handlers.
  var NativeRTCPeerConnection =
    window.RTCPeerConnection || window.webkitRTCPeerConnection;
  if (typeof NativeRTCPeerConnection === 'function') {
    var Wrapped;
    try {
      Wrapped = class extends NativeRTCPeerConnection {
        constructor(config, constraints) {
          super(config, constraints);
          try {
            this.addEventListener('track', function (ev) {
              try { if (ev && ev.track) tapTrack(ev.track); } catch (e) {}
            });
          } catch (e) {}
        }
      };
    } catch (e) {
      Wrapped = null; // class syntax unavailable — leave the native ctor untouched
    }
    if (Wrapped) {
      try {
        Object.defineProperty(window, 'RTCPeerConnection', {
          value: Wrapped, writable: true, configurable: true
        });
        if (window.webkitRTCPeerConnection) window.webkitRTCPeerConnection = Wrapped;
      } catch (e) {}
    }
  }

  // Open a capture window: reset the ring + counters and start appending. The tap
  // itself is installed at PC construction; samples are retained only while
  // `capturing` is true (so pre-start audio is discarded, per the start/stop D8).
  function startCapture() {
    capturing = true;
    ring = [];
    ringSamples = 0;
    drainCursorChunk = 0;
    drainCursorOffset = 0;
    droppedFrames = 0;
    // NOTE: tappedTracks is a LIFETIME counter — intentionally NOT reset here, so a
    // track connected before this window still marks the session as "connected" (I1).
    injectedLeaked = false;
    bufferCapHit = false;
    return { ok: true, sample_rate: sourceRate };
  }

  // Close the capture window. Readers stay attached (they only append while
  // `capturing`, so a still-live track can be re-captured by a later start/stop
  // pair); the per-track 'ended' teardown is what releases a reader's stream lock
  // on renegotiation (I4). The shim then drains what was buffered.
  function stopCapture() {
    capturing = false;
    return { ok: true };
  }

  // Return up to `maxBytes` bytes of buffered mono f32 (little-endian) from the
  // drain cursor as base64, advancing the cursor (D3 bounded exfil). Reports the
  // native `sample_rate`, cumulative `dropped_frames`, the A2 `injected_leaked`
  // tripwire, the `buffer_cap_hit` flag, and whether `more` remains.
  function drain(maxBytes) {
    var budgetSamples = Math.max(0, Math.floor((maxBytes || 0) / 4));
    var out = [];
    var taken = 0;
    while (taken < budgetSamples && drainCursorChunk < ring.length) {
      var chunk = ring[drainCursorChunk];
      var avail = chunk.length - drainCursorOffset;
      var want = Math.min(avail, budgetSamples - taken);
      for (var i = 0; i < want; i++) out.push(chunk[drainCursorOffset + i]);
      drainCursorOffset += want;
      taken += want;
      if (drainCursorOffset >= chunk.length) {
        drainCursorChunk++;
        drainCursorOffset = 0;
      }
    }
    // Serialise the taken f32 samples to little-endian bytes → base64.
    var f32 = new Float32Array(out);
    var bytes = new Uint8Array(f32.buffer);
    var bin = '';
    for (var b = 0; b < bytes.length; b++) bin += String.fromCharCode(bytes[b]);
    // If the budget was too small to advance the cursor at all (maxBytes < 4), do
    // NOT report `more:true` with no progress — that would spin the shim's drain
    // loop to its deadline (MINOR 3). Cursor-stuck ⇒ done.
    var more = (taken > 0) && (drainCursorChunk < ring.length);
    return {
      samples_b64: (bytes.length ? btoa(bin) : ''),
      sample_rate: sourceRate,
      dropped_frames: droppedFrames,
      tapped_tracks: tappedTracks,   // I1: 0 → no_inbound_track vs no_samples
      injected_leaked: injectedLeaked,
      buffer_cap_hit: bufferCapHit,
      more: more
    };
  }

  var api = {
    nonce: NONCE,
    enqueue: enqueue,
    pending: pending,
    isInjectedTrack: isInjectedTrack,
    startCapture: startCapture,
    stopCapture: stopCapture,
    drain: drain,
    ready: true
  };

  // FND-0019: expose the API non-enumerable / non-configurable / non-writable.
  Object.defineProperty(window, API_KEY, {
    value: api,
    enumerable: false,
    configurable: false,
    writable: false
  });
})();
