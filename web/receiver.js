// PlayTogether receiver.
//
// Every audio packet says "play me at host time T". This page:
//  1. keeps an estimate of (host clock − this page's performance.now()) via ping/pong,
//  2. maps performance.now() onto the AudioContext clock at the moment sound leaves the speaker,
//  3. schedules each chunk so it is *heard* at T (minus this device's nudge),
//  4. nudges playbackRate by up to ±0.3% to cancel clock drift without audible jumps,
//  5. loops a silent <audio> so phones keep the page running with the screen off or in another app.
"use strict";

const HEADER = 16;
const RATE = 48000;
const NEED_MS = 8;          // safety margin this page wants before a chunk is due
const RESYNC_S = 0.015;     // larger errors jump instead of drifting back
const MAX_RATE_ADJ = 0.003;
const STALL_MS = 1000;      // packets come every ~5 ms; a gap this long means the page was frozen or asleep
const CATCH_UP_MS = 3000;   // after a stall, how long stale late packets are dropped without counting them

const $ = (id) => document.getElementById(id);

const store = {
  get(key, fallback) {
    try { const v = localStorage.getItem("gt." + key); return v === null ? fallback : JSON.parse(v); }
    catch { return fallback; }
  },
  set(key, value) {
    try { localStorage.setItem("gt." + key, JSON.stringify(value)); } catch { /* private mode */ }
  },
};

function defaultName() {
  const ua = navigator.userAgent;
  const kind = /iPhone/.test(ua) ? "iPhone" : /iPad/.test(ua) ? "iPad" : /Android/.test(ua) ? "Android"
    : /Mac OS X/.test(ua) ? "Mac" : /Windows/.test(ua) ? "Windows PC" : /Linux/.test(ua) ? "Linux PC" : "Device";
  return `${kind} ${Math.floor(Math.random() * 90 + 10)}`;
}

function deviceKind() {
  const ua = navigator.userAgent;
  const touchMac = /Macintosh/.test(ua) && navigator.maxTouchPoints > 1; // iPadOS
  if (/iPad|Tablet/.test(ua) || touchMac || (/Android/.test(ua) && !/Mobile/.test(ua))) return "tablet";
  if (/iPhone|iPod|Android|Mobile/.test(ua)) return "phone";
  return "computer";
}

// Lets the host give this device the same seat when it reconnects.
const DEVICE_ID = store.get("deviceId", null) || Math.random().toString(36).slice(2) + Date.now().toString(36);
store.set("deviceId", DEVICE_ID);

const settings = {
  name: store.get("name", null) || defaultName(),
  volume: store.get("volume", 1),
  offsetMs: store.get("offsetMs", 0),
  muted: false,
};
store.set("name", settings.name);

let ws = null;
let isHost = false;
let joined = false;
let ctx = null;
let gain = null;

// host_ms − perf_ms, from the lowest-latency recent ping.
const clock = { offset: null, rtt: null, samples: [] };
// AudioContext seconds − performance seconds, for the moment a sample is heard.
let ctxMinusPerf = null;

let nextSeq = -1;
let nextStart = null;       // AudioContext time where the next contiguous chunk should start
let late = 0;
let lastLateAt = -1e9;
let packets = 0;
let lastPacketAt = 0;       // Date.now(), which unlike performance.now() keeps counting while a phone sleeps
let catchUpUntil = 0;
let lastPlayEndUs = null;   // to spot room-delay changes in the packet timestamps
let keepAlive = null;       // the silent <audio>, see startKeepAlive()
let keepAliveWorks = false;

// Min slack over a sliding ~2 s window, kept as 500 ms buckets.
const slack = { cur: Infinity, buckets: [] };

// ---------- connection ----------

function connect() {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  ws = new WebSocket(`${proto}://${location.host}/ws`);
  ws.binaryType = "arraybuffer";
  setConn("connecting…", "");
  ws.onopen = () => {
    setConn("connected", "good");
    send({ type: "hello", device_id: DEVICE_ID, kind: deviceKind() });
    clock.samples = [];
    for (let i = 0; i < 15; i++) setTimeout(ping, i * 40);
    if (joined) send({ type: "start" });
  };
  ws.onclose = () => {
    setConn("reconnecting…", "bad");
    nextStart = null;
    lastPacketAt = 0; // a new connection resyncs in onopen and starts without a backlog
    setTimeout(connect, 1000);
  };
  ws.onmessage = (e) => {
    if (typeof e.data === "string") onControl(JSON.parse(e.data));
    else onAudio(e.data);
  };
}

function send(obj) {
  if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(obj));
}

function ping() {
  send({ type: "ping", c: performance.now() });
}

function onControl(m) {
  if (m.type === "pong") onPong(m);
  else if (m.type === "layout") renderSpot(m);
  else if (m.type === "flash") flashAt(m.at_us, m.dur_ms);
  else if (m.type === "hello") {
    isHost = m.is_host;
    $("host-warning").hidden = !isHost;
    $("join").hidden = isHost || joined;
  } else if (m.type === "set") {
    if (m.volume != null) settings.volume = m.volume;
    if (m.offset_ms != null) settings.offsetMs = m.offset_ms;
    if (m.muted != null) settings.muted = m.muted;
    saveAndApply();
  }
}

function onPong(m) {
  const t1 = performance.now();
  const rtt = t1 - m.c;
  if (rtt < 0) return;
  clock.samples.push({ rtt, off: m.h / 1000 - (m.c + t1) / 2 });
  if (clock.samples.length > 12) clock.samples.shift();
  const best = clock.samples.reduce((a, b) => (b.rtt < a.rtt ? b : a));
  clock.rtt = best.rtt;
  if (clock.offset === null || Math.abs(best.off - clock.offset) > 20) clock.offset = best.off;
  else clock.offset += (best.off - clock.offset) * 0.25;
}

// ---------- audio ----------

async function join() {
  if (isHost) return;
  try { if (navigator.audioSession) navigator.audioSession.type = "playback"; } catch { /* iOS < 17 */ }
  startKeepAlive(); // before any await: media may only start inside the tap
  if (!ctx) {
    const Ctx = window.AudioContext || window.webkitAudioContext;
    try { ctx = new Ctx({ latencyHint: "interactive", sampleRate: RATE }); }
    catch { ctx = new Ctx({ latencyHint: "interactive" }); }
    gain = ctx.createGain();
    gain.connect(ctx.destination);
    ctx.onstatechange = updateStatus;
  }
  await ctx.resume();
  // iOS only unlocks audio after something actually plays inside the tap.
  const unlock = ctx.createBufferSource();
  unlock.buffer = ctx.createBuffer(1, 1, ctx.sampleRate);
  unlock.connect(ctx.destination);
  unlock.start();

  joined = true;
  ctxMinusPerf = null;
  nextStart = null;
  applyGain();
  send({ type: "start" });
  setTimeout(sendStats, 500); // let the host size the room delay for us quickly
  $("join").hidden = true;
  $("player").hidden = false;
  requestWakeLock();
}

async function requestWakeLock() {
  try { await navigator.wakeLock?.request("screen"); } catch { /* needs https; audio plays with the screen off anyway */ }
}

// ---------- staying alive in the background ----------
//
// Phones treat a page that only uses Web Audio as idle once it's hidden: Android lets Chrome be
// frozen soon after the screen goes off or you switch apps, and iOS suspends the AudioContext.
// A playing <audio> element is what they keep running (it's how music sites play from the lock
// screen), so we loop a silent one while joined. The real sound still goes through Web Audio,
// which is what keeps the timing exact. Pausing it from the lock screen, or the system pausing it
// (a call, another app taking over audio), silences this device until it's played again.

function startKeepAlive() {
  if (!keepAlive) {
    // Over 5 s, or Chrome treats it as a short sound effect and doesn't keep the page alive for it.
    keepAlive = new Audio(silentWav(15));
    keepAlive.loop = true;
    keepAlive.addEventListener("playing", () => {
      keepAliveWorks = true;
      if (ctx && ctx.state !== "running") ctx.resume().catch(() => {}); // the lock screen's play button
      applyGain();
      updateStatus();
    });
    keepAlive.addEventListener("pause", () => { applyGain(); updateStatus(); });
    if ("mediaSession" in navigator) {
      try { navigator.mediaSession.metadata = new MediaMetadata({ title: "PlayTogether", artist: "Room audio, in sync" }); }
      catch { /* no MediaMetadata */ }
      try { navigator.mediaSession.setPositionState({ duration: Infinity }); } // live: hide the silent loop's seek bar
      catch { /* not supported */ }
    }
  }
  keepAlive.play().catch(() => { /* unsupported: Web Audio still plays while the page is visible */ });
}

// Only counts once it has actually played, so a browser that can't play it isn't silenced.
function keepAlivePaused() {
  return keepAliveWorks && keepAlive.paused;
}

// `seconds` of 8 kHz 8-bit mono silence (8-bit PCM is unsigned, so silence is 128).
function silentWav(seconds) {
  const rate = 8000, n = rate * seconds;
  const v = new DataView(new ArrayBuffer(44 + n));
  const text = (at, s) => { for (let i = 0; i < s.length; i++) v.setUint8(at + i, s.charCodeAt(i)); };
  text(0, "RIFF"); v.setUint32(4, 36 + n, true); text(8, "WAVE");
  text(12, "fmt "); v.setUint32(16, 16, true);
  v.setUint16(20, 1, true); v.setUint16(22, 1, true);     // PCM, mono
  v.setUint32(24, rate, true); v.setUint32(28, rate, true); // sample rate, bytes per second
  v.setUint16(32, 1, true); v.setUint16(34, 8, true);     // bytes per frame, bits per sample
  text(36, "data"); v.setUint32(40, n, true);
  new Uint8Array(v.buffer, 44).fill(128);
  return URL.createObjectURL(new Blob([v.buffer], { type: "audio/wav" }));
}

// A long gap in packets means the browser froze this page or the phone slept. performance.now()
// stops during sleep, so the clock offset may be stale. And what arrives first is a backlog of
// audio that's long overdue, which isn't this network being slow: drop it quietly instead of
// reporting it, or the host would raise the room delay for everyone.
function afterStall(wall) {
  clock.samples = [];
  nextStart = null;
  ctxMinusPerf = null;
  catchUpUntil = wall + CATCH_UP_MS;
  for (let i = 0; i < 6; i++) ping();
}

// Coming back to the page: pick up again without a tap where the browser allows it, unless the
// user (or the system) paused us from the lock screen.
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState !== "visible" || !joined) return;
  if (ctx.state !== "running" && !keepAlivePaused()) ctx.resume().catch(() => {});
  requestWakeLock(); // browsers drop it whenever the page is hidden
});

function updateMapping() {
  let m = null;
  if (ctx.getOutputTimestamp) {
    const ts = ctx.getOutputTimestamp();
    if (ts.contextTime > 0 && ts.performanceTime > 0) m = ts.contextTime - ts.performanceTime / 1000;
  }
  if (m === null) {
    m = ctx.currentTime - performance.now() / 1000 - (ctx.outputLatency || ctx.baseLatency || 0);
  }
  if (ctxMinusPerf === null || Math.abs(m - ctxMinusPerf) > 0.03) ctxMinusPerf = m;
  else ctxMinusPerf += (m - ctxMinusPerf) * 0.01;
}

function onAudio(buf) {
  const wall = Date.now();
  if (lastPacketAt > 0 && wall - lastPacketAt > STALL_MS) afterStall(wall);
  packets++;
  lastPacketAt = wall;
  if (packets % 100 === 0) ping(); // timers get throttled in background tabs; packets don't
  if (!joined || !ctx || ctx.state !== "running" || clock.samples.length < 5) return;

  const dv = new DataView(buf);
  const channels = dv.getUint8(1); // 2 = normal stereo, 1 = this device's 3D share
  const frames = dv.getUint16(2, true);
  const seq = dv.getUint32(4, true);
  const playAtUs = dv.getFloat64(8, true);

  // A jump in the timeline means the room delay changed: older slack readings no longer apply.
  if (lastPlayEndUs !== null && Math.abs(playAtUs - lastPlayEndUs) > 1000) {
    slack.cur = Infinity;
    slack.buckets = [];
  }
  lastPlayEndUs = playAtUs + (frames / RATE) * 1e6;

  updateMapping();
  const heardPerfMs = playAtUs / 1000 - clock.offset - settings.offsetMs;
  const ideal = heardPerfMs / 1000 + ctxMinusPerf;
  const now = ctx.currentTime;
  const earliest = now + 0.002;
  if (wall < catchUpUntil) {
    if (ideal < earliest) return; // stale backlog from before a stall, see afterStall()
    catchUpUntil = 0;
  }
  const s = ideal - now;
  if (s < slack.cur) slack.cur = s;

  const dur = frames / RATE;
  let start = ideal;
  let rate = 1;
  if (seq === nextSeq && nextStart !== null && Math.abs(nextStart - ideal) < RESYNC_S) {
    // Positive error: we're running behind the ideal schedule, so speed up a touch.
    const err = nextStart - ideal;
    rate = 1 + Math.max(-MAX_RATE_ADJ, Math.min(MAX_RATE_ADJ, err));
    start = nextStart;
  }

  let skip = 0;
  if (start < earliest) {
    late++;
    lastLateAt = performance.now();
    skip = earliest - start;
    start = earliest;
    if (skip >= dur) { nextStart = null; nextSeq = (seq + 1) >>> 0; return; }
  }

  const pcm = new Int16Array(buf, HEADER, frames * channels);
  const ab = ctx.createBuffer(channels, frames, RATE);
  for (let c = 0; c < channels; c++) {
    const out = ab.getChannelData(c);
    for (let i = 0, j = c; i < frames; i++, j += channels) out[i] = pcm[j] / 32768;
  }
  const src = ctx.createBufferSource();
  src.buffer = ab;
  src.playbackRate.value = rate;
  src.connect(gain);
  src.start(start, skip);

  nextStart = start + (dur - skip) / rate;
  nextSeq = (seq + 1) >>> 0;
}

function applyGain() {
  if (!gain) return;
  const silent = settings.muted || keepAlivePaused();
  gain.gain.setTargetAtTime(silent ? 0 : settings.volume, ctx.currentTime, 0.02);
}

// ---------- UI ----------

function clockOf(angle) {
  const halfHours = Math.round((((angle % 360) + 360) % 360) / 15);
  const hour = Math.floor(halfHours / 2) % 12 || 12;
  return `${hour}${halfHours % 2 ? ":30" : ""} o'clock`;
}

const MODE_TEXT = {
  off: "3D is off right now: every device plays the normal mix. Your spot matters when the host turns 3D on.",
  surround: "3D: surround. Sound comes from all around you.",
  rotate: "3D: the music is circling around the room.",
  position: "3D: the host is placing the music around the room.",
};

function renderSpot(m) {
  if (isHost) return;
  const me = m.devices.find((d) => d.id === m.you);
  $("spot").hidden = false;
  $("spot-clock").textContent = me ? clockOf(me.angle) : "—";
  $("spot-hint").textContent = me
    ? "12 o'clock is the host PC's screen, and you sit in the middle. Same distance from you as the other devices, speaker facing you."
    : "Tap Join to get a spot in the circle.";
  $("spot-mode").textContent = MODE_TEXT[m.mode] || "";

  const R = 44;
  const pos = (a) => [R * Math.sin((a * Math.PI) / 180), -R * Math.cos((a * Math.PI) / 180)];
  let svg = `<circle r="${R}" class="ring"/><text y="-55" class="front">screen</text><circle r="4" class="you"/>`;
  for (const d of m.devices) {
    const [x, y] = pos(d.angle);
    const mine = d.id === m.you;
    svg += `<circle cx="${x.toFixed(1)}" cy="${y.toFixed(1)}" r="${mine ? 8 : 5}" class="${mine ? "dot me" : "dot"}"/>`;
    if (d.id === "pc") svg += `<text x="${x.toFixed(1)}" y="${(y + 2.5).toFixed(1)}" class="pc">PC</text>`;
  }
  $("spot-map").innerHTML = svg;
}

// Speaker test: light up exactly when this device's chime is heard.
function flashAt(atUs, durMs) {
  if (clock.offset === null) return;
  const at = atUs / 1000 - clock.offset - settings.offsetMs;
  setTimeout(() => {
    document.body.classList.add("flash");
    setTimeout(() => document.body.classList.remove("flash"), durMs);
  }, Math.max(0, at - performance.now()));
}

function setConn(text, cls) {
  const el = $("conn");
  el.textContent = text;
  el.className = "pill " + cls;
}

function saveAndApply() {
  store.set("volume", settings.volume);
  store.set("offsetMs", settings.offsetMs);
  store.set("name", settings.name);
  applyGain();
  renderControls();
}

function renderControls() {
  $("vol").value = Math.round(settings.volume * 100);
  $("vol-out").textContent = `${Math.round(settings.volume * 100)}%`;
  $("off").value = settings.offsetMs;
  $("off-out").textContent = `${settings.offsetMs > 0 ? "+" : ""}${settings.offsetMs} ms`;
  $("mute").textContent = settings.muted ? "Unmute" : "Mute";
  $("mute").classList.toggle("on", settings.muted);
  if (document.activeElement !== $("name")) $("name").value = settings.name;
}

function minSlack() {
  return Math.min(slack.cur, ...slack.buckets);
}

function updateStatus() {
  const el = $("status");
  const receiving = Date.now() - lastPacketAt < 500;
  const paused = joined && (ctx.state !== "running" || keepAlivePaused());
  if (joined) {
    // Also hides it again when the browser resumes us without a tap.
    $("join").hidden = !paused;
    $("join").textContent = "Tap to resume";
  }
  let text = "Playing in sync", cls = "good";
  if (paused) { text = "Paused, tap to resume"; cls = "warn"; }
  else if (clock.samples.length < 5) { text = "Syncing clock…"; cls = "warn"; }
  else if (!receiving) { text = "Waiting for audio…"; cls = "warn"; }
  else if (performance.now() - lastLateAt < 2000) { text = "Catching up, the room delay is adjusting…"; cls = "warn"; }
  el.textContent = text;
  el.className = "status " + cls;

  const ms = minSlack();
  $("s-rtt").textContent = clock.rtt === null ? "—" : `${clock.rtt.toFixed(1)} ms`;
  $("s-head").textContent = Number.isFinite(ms) ? `${(ms * 1000 - NEED_MS).toFixed(1)} ms` : "—";
  const outLat = ctx ? (ctx.outputLatency || ctx.baseLatency || 0) * 1000 : null;
  $("s-lat").textContent = outLat === null ? "—" : `${outLat.toFixed(1)} ms`;
  $("s-late").textContent = late;
}

$("join").onclick = join;
$("mute").onclick = () => { settings.muted = !settings.muted; saveAndApply(); };
$("vol").oninput = (e) => { settings.volume = e.target.value / 100; saveAndApply(); };
$("off").oninput = (e) => { settings.offsetMs = Number(e.target.value); saveAndApply(); };
$("name").oninput = (e) => { settings.name = e.target.value.trim() || defaultName(); store.set("name", settings.name); };
for (const b of document.querySelectorAll(".nudge button")) {
  b.onclick = () => {
    settings.offsetMs = Math.max(-300, Math.min(500, settings.offsetMs + Number(b.dataset.d)));
    saveAndApply();
  };
}

setInterval(() => {
  slack.buckets.push(slack.cur);
  slack.cur = Infinity;
  if (slack.buckets.length > 4) slack.buckets.shift();
  ping();
}, 500);

function sendStats() {
  const ms = minSlack();
  const playing = joined && ctx.state === "running" && !keepAlivePaused() && Date.now() - lastPacketAt < 1000;
  send({
    type: "stats",
    name: settings.name,
    playing,
    rtt_ms: clock.rtt,
    headroom_ms: playing && Number.isFinite(ms) ? ms * 1000 - NEED_MS : null,
    late,
    out_latency_ms: ctx ? (ctx.outputLatency || ctx.baseLatency || 0) * 1000 : null,
    volume: settings.volume,
    offset_ms: settings.offsetMs,
    muted: settings.muted,
  });
}

setInterval(() => { sendStats(); updateStatus(); }, 1000);

renderControls();
connect();
