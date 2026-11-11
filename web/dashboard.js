"use strict";

const $ = (id) => document.getElementById(id);
let ws = null;
let selectedUrl = null;
const rows = new Map(); // client id → { el, refs }
let last = null;           // latest state from the host
let drag = null;           // { kind: "seat", id, angle } | { kind: "sound" }
let soundTarget = null;    // where the user is dragging the sound dot
let lastThetaAt = 0;

const deg2xy = (a, r) => [r * Math.sin((a * Math.PI) / 180), -r * Math.cos((a * Math.PI) / 180)];
const norm = (a) => ((a % 360) + 360) % 360;

function clockOf(angle) {
  const halfHours = Math.round(norm(angle) / 15);
  const hour = Math.floor(halfHours / 2) % 12 || 12;
  return `${hour}${halfHours % 2 ? ":30" : ""} o'clock`;
}

function connect() {
  ws = new WebSocket(`ws://${location.host}/ws/control`);
  ws.onopen = () => setPill($("conn"), "live", "good");
  ws.onclose = () => { setPill($("conn"), "host offline", "bad"); setTimeout(connect, 1000); };
  ws.onmessage = (e) => render(JSON.parse(e.data));
}

function send(cmd) {
  if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(cmd));
}

function setPill(el, text, cls) {
  el.textContent = text;
  el.className = "pill " + cls;
}

// Don't overwrite a control while the user is dragging or typing in it.
function setValue(input, value) {
  if (document.activeElement !== input) input.value = value;
}

const fmtMs = (v) => (v == null ? "—" : `${v.toFixed(1)} ms`);
const signed = (v) => `${v > 0 ? "+" : ""}${Math.round(v)} ms`;

function render(s) {
  last = s;
  lastThetaAt = performance.now();
  renderSpatial(s);
  renderRoom(s);

  // Join
  if (!selectedUrl || !s.join_urls.includes(selectedUrl)) selectedUrl = s.join_urls[0] || null;
  const urls = $("urls");
  if (urls.dataset.key !== s.join_urls.join() + selectedUrl) {
    urls.dataset.key = s.join_urls.join() + selectedUrl;
    urls.replaceChildren(...s.join_urls.map((u) => {
      const b = document.createElement("button");
      b.textContent = u.replace(/^http:\/\//, "").replace(/\/$/, "");
      b.className = u === selectedUrl ? "sel" : "";
      b.onclick = () => { selectedUrl = u; render(s); };
      return b;
    }));
    $("qr").src = selectedUrl ? `/qr.svg?text=${encodeURIComponent(selectedUrl)}` : "";
  }

  // Room delay
  $("delay").textContent = Math.round(s.delay.ms);
  $("auto").checked = s.delay.auto;
  setValue($("delay-slider"), Math.round(s.delay.ms));
  const db = s.level > 0 ? 20 * Math.log10(s.level) : -100;
  $("level").style.width = `${Math.max(0, Math.min(100, ((db + 60) / 60) * 100))}%`;
  $("capture-hint").textContent = s.capturing
    ? "Volume keys on this PC set the volume for the whole room."
    : "Not capturing yet. Is PipeWire running?";

  // This PC
  const pc = s.pc;
  $("pc-mute").textContent = pc.muted ? "Unmute PC speakers" : "Mute PC speakers";
  $("pc-mute").classList.toggle("on", pc.muted);
  const pcStatus = $("pc-status");
  pcStatus.textContent = pc.muted ? "Muted (receivers only)" : "Playing in sync";
  pcStatus.className = "status " + (pc.muted ? "warn" : "good");
  setValue($("pc-vol"), Math.round(pc.volume * 100));
  $("pc-vol-out").textContent = `${Math.round(pc.volume * 100)}%`;
  setValue($("pc-off"), Math.round(pc.offset_ms));
  $("pc-off-out").textContent = signed(pc.offset_ms);
  $("pc-output").textContent = pc.output;
  $("pc-output").title = pc.output;
  $("pc-lat").textContent = fmtMs(pc.out_latency_ms);
  $("pc-head").textContent = pc.muted ? "—" : fmtMs(pc.headroom_ms);
  $("pc-under").textContent = pc.underruns;

  // Devices
  const seen = new Set();
  for (const c of s.clients) {
    seen.add(c.id);
    let row = rows.get(c.id);
    if (!row) {
      row = makeRow(c.id);
      rows.set(c.id, row);
      $("devices").append(row.el);
    }
    updateRow(row, c);
  }
  for (const [id, row] of rows) {
    if (!seen.has(id)) { row.el.remove(); rows.delete(id); }
  }
  const receivers = s.clients.filter((c) => !c.is_host);
  $("count").textContent = receivers.length ? `(${receivers.length})` : "";
  $("no-devices").hidden = s.clients.length > 0;
}

// ---------- 3D sound ----------

const MODE_HINTS = {
  off: "Every device plays the normal stereo mix.",
  rotate: "The music circles around you, device by device.",
  position: "Drag the glowing dot on the room map to put the music anywhere around you.",
};

function renderSpatial(s) {
  const st = s.spatial.settings;
  for (const b of document.querySelectorAll("#modes button")) b.classList.toggle("sel", b.dataset.mode === st.mode);
  $("mode-hint").textContent = st.mode === "surround"
    ? s.spatial.multichannel
      ? "Movie surround detected (5.1/7.1): every channel plays from its own direction."
      : "Stereo spread: left and right wrap around the front, the room's ambience comes from behind. Play a 5.1/7.1 movie (e.g. in VLC) for real surround."
    : MODE_HINTS[st.mode];
  $("rotate-controls").hidden = st.mode !== "rotate";
  $("width-controls").hidden = st.mode !== "rotate" && st.mode !== "position";
  setValue($("lap"), st.lap_s);
  $("lap-out").textContent = `${st.lap_s} s`;
  $("dir").textContent = st.clockwise ? "Clockwise ↻" : "Anticlockwise ↺";
  setValue($("width"), st.width_deg);
  $("width-out").textContent = `${Math.round(st.width_deg)}°`;
  $("test").disabled = s.spatial.testing || s.seats.length === 0;
  $("test").textContent = s.spatial.testing ? "Testing…" : "Speaker test";

  const guide = [...s.seats].sort((a, b) => a.angle - b.angle);
  $("guide").replaceChildren(...guide.map((seat) => {
    const li = document.createElement("li");
    const where = document.createElement("b");
    where.textContent = clockOf(seat.angle);
    li.append(where, ` ${seat.name}${seat.id === "pc" ? " (front, the screen)" : ""}`);
    return li;
  }));
  $("layout-hint").textContent = s.seats.length < 3
    ? "Join more devices: 3D needs at least 3, and 4–8 sounds best."
    : s.spatial.custom
      ? "Your own layout. Drag devices to where they really stand, or Arrange automatically."
      : "Arranged automatically: best speakers at the front, phones at the sides and back. Drag to adjust.";
}

// ---------- room map ----------

const SEAT_R = 112;
const SOUND_R = 72;
const svg = $("room");
svg.innerHTML = `
  <circle r="${SEAT_R}" class="ring"/>
  <circle r="${SOUND_R}" class="ring inner"/>
  <text y="-150" class="front">▲ screen (front)</text>
  <g class="you"><circle r="13"/><text y="4">you</text></g>
  <g id="sound-dot"><circle r="11" class="glow"/><circle r="7"/></g>
  <g id="seats"></g>`;
const seatsLayer = svg.querySelector("#seats");
const soundDot = svg.querySelector("#sound-dot");
const seatEls = new Map();

function seatEl(id) {
  let g = seatEls.get(id);
  if (!g) {
    g = document.createElementNS("http://www.w3.org/2000/svg", "g");
    g.classList.add("seat");
    g.dataset.seat = String(id);
    g.innerHTML = `<circle r="17"/><text class="initial" y="5"></text><text class="label"></text><text class="clock"></text>`;
    seatsLayer.append(g);
    seatEls.set(id, g);
  }
  return g;
}

function renderRoom(s) {
  const seen = new Set();
  for (const seat of s.seats) {
    seen.add(seat.id);
    const g = seatEl(seat.id);
    const angle = drag?.kind === "seat" && drag.id === seat.id && drag.angle != null ? drag.angle : seat.angle;
    placeSeat(g, seat, angle);
  }
  for (const [id, g] of seatEls) {
    if (!seen.has(id)) { g.remove(); seatEls.delete(id); }
  }
}

function placeSeat(g, seat, angle) {
  const [x, y] = deg2xy(angle, SEAT_R);
  g.setAttribute("transform", `translate(${x.toFixed(1)} ${y.toFixed(1)})`);
  g.classList.toggle("pc", seat.id === "pc");
  g.querySelector(".initial").textContent = seat.id === "pc" ? "PC" : seat.name.slice(0, 2);
  // Name and clock position sit centred under each seat.
  const label = g.querySelector(".label");
  const clockText = g.querySelector(".clock");
  for (const [el, y] of [[label, 31], [clockText, 43]]) {
    el.setAttribute("x", "0");
    el.setAttribute("y", String(y));
    el.setAttribute("text-anchor", "middle");
  }
  label.textContent = seat.name.length > 14 ? seat.name.slice(0, 13) + "…" : seat.name;
  clockText.textContent = clockOf(angle);
}

function animateSound() {
  requestAnimationFrame(animateSound);
  if (!last) return;
  const sp = last.spatial;
  const mode = sp.settings.mode;
  soundDot.style.display = mode === "rotate" || mode === "position" ? "" : "none";
  let theta = sp.theta;
  if (mode === "rotate") {
    // The host reports where capture is; extrapolate smoothly and show what's being heard (D later).
    const dir = sp.settings.clockwise ? 1 : -1;
    const lagS = (performance.now() - lastThetaAt) / 1000 - last.delay.ms / 1000;
    theta += (dir * 360 * lagS) / sp.settings.lap_s;
  } else if (mode === "position" && soundTarget !== null) {
    theta = soundTarget;
  }
  const [x, y] = deg2xy(theta, SOUND_R);
  soundDot.setAttribute("transform", `translate(${x.toFixed(1)} ${y.toFixed(1)})`);
}
requestAnimationFrame(animateSound);

function angleFromEvent(e) {
  const pt = svg.createSVGPoint();
  pt.x = e.clientX;
  pt.y = e.clientY;
  const p = pt.matrixTransform(svg.getScreenCTM().inverse());
  return norm((Math.atan2(p.x, -p.y) * 180) / Math.PI);
}

let lastSoundSend = 0;
function moveSound(e) {
  soundTarget = Math.round(angleFromEvent(e));
  const now = performance.now();
  if (now - lastSoundSend > 40) {
    lastSoundSend = now;
    send({ cmd: "spatial", target_deg: soundTarget });
  }
}

svg.addEventListener("pointerdown", (e) => {
  const seat = e.target.closest("[data-seat]");
  if (seat) {
    const id = seat.dataset.seat === "pc" ? "pc" : Number(seat.dataset.seat);
    drag = { kind: "seat", id, angle: null };
  } else if (last?.spatial.settings.mode === "position") {
    drag = { kind: "sound" };
    moveSound(e);
  } else {
    return;
  }
  svg.setPointerCapture(e.pointerId);
  e.preventDefault();
});
svg.addEventListener("pointermove", (e) => {
  if (!drag) return;
  if (drag.kind === "sound") return moveSound(e);
  drag.angle = Math.round(angleFromEvent(e) / 5) * 5 % 360;
  if (last) renderRoom(last);
});
const endDrag = () => {
  if (drag?.kind === "seat" && drag.angle != null) send({ cmd: "place", member: drag.id, angle: drag.angle });
  if (drag?.kind === "sound") send({ cmd: "spatial", target_deg: soundTarget });
  drag = null;
  setTimeout(() => { if (!drag) soundTarget = null; }, 800);
};
svg.addEventListener("pointerup", endDrag);
svg.addEventListener("pointercancel", endDrag);

// ---------- devices ----------

function makeRow(id) {
  const el = document.createElement("div");
  el.className = "device";
  el.innerHTML = `
    <div class="row">
      <div style="min-width:0"><div class="name"></div><div class="ip"></div></div>
      <span class="pill state"></span>
    </div>
    <dl class="stats">
      <dt>Seat</dt><dd class="seat-pos"></dd>
      <dt>Headroom</dt><dd class="head"></dd>
      <dt>Round trip</dt><dd class="rtt"></dd>
      <dt>Output latency</dt><dd class="lat"></dd>
      <dt>Late chunks</dt><dd class="late"></dd>
    </dl>
    <div class="controls">
      <label>Volume <output class="vol-out"></output></label>
      <input type="range" class="vol" min="0" max="100" step="1">
      <label>Sync nudge <output class="off-out"></output></label>
      <input type="range" class="off" min="-300" max="500" step="1">
      <div class="row" style="margin-top:10px"><button class="identify">Identify</button><button class="mute">Mute</button></div>
    </div>`;
  const q = (sel) => el.querySelector(sel);
  const refs = {
    name: q(".name"), ip: q(".ip"), state: q(".state"), head: q(".head"), rtt: q(".rtt"),
    lat: q(".lat"), late: q(".late"), vol: q(".vol"), volOut: q(".vol-out"), off: q(".off"),
    offOut: q(".off-out"), mute: q(".mute"), controls: q(".controls"), muted: false,
    seat: q(".seat-pos"), identify: q(".identify"),
  };
  refs.identify.onclick = () => send({ cmd: "test", member: id });
  refs.vol.oninput = () => send({ cmd: "client", id, volume: refs.vol.value / 100 });
  refs.off.oninput = () => send({ cmd: "client", id, offset_ms: Number(refs.off.value) });
  refs.mute.onclick = () => send({ cmd: "client", id, muted: !refs.muted });
  return { el, refs };
}

function updateRow({ refs }, c) {
  const st = c.stats;
  refs.name.textContent = st.name || `Device ${c.id}`;
  refs.ip.textContent = c.ip;
  refs.controls.hidden = c.is_host;
  if (c.is_host) setPill(refs.state, "host browser (not playing)", "");
  else if (c.stale) setPill(refs.state, "not responding", "bad");
  else if (!st.playing) setPill(refs.state, "connected, not joined", "warn");
  else if (st.headroom_ms != null && st.headroom_ms < 0) setPill(refs.state, "struggling", "warn");
  else setPill(refs.state, st.muted ? "muted" : "playing", "good");
  refs.seat.textContent = c.angle == null ? "—" : clockOf(c.angle);
  refs.identify.disabled = c.angle == null;
  refs.head.textContent = fmtMs(st.headroom_ms);
  refs.rtt.textContent = fmtMs(st.rtt_ms);
  refs.lat.textContent = fmtMs(st.out_latency_ms);
  refs.late.textContent = st.late;
  setValue(refs.vol, Math.round(st.volume * 100));
  refs.volOut.textContent = `${Math.round(st.volume * 100)}%`;
  setValue(refs.off, Math.round(st.offset_ms));
  refs.offOut.textContent = signed(st.offset_ms);
  refs.muted = st.muted;
  refs.mute.textContent = st.muted ? "Unmute" : "Mute";
  refs.mute.classList.toggle("on", st.muted);
}

$("auto").onchange = (e) => send({ cmd: "delay", auto: e.target.checked });
$("delay-slider").oninput = (e) => {
  $("auto").checked = false;
  send({ cmd: "delay", auto: false, ms: Number(e.target.value) });
};
$("pc-mute").onclick = () => send({ cmd: "pc", muted: !$("pc-mute").classList.contains("on") });
$("pc-vol").oninput = (e) => send({ cmd: "pc", volume: e.target.value / 100 });
$("pc-off").oninput = (e) => send({ cmd: "pc", offset_ms: Number(e.target.value) });
for (const b of document.querySelectorAll("#modes button")) b.onclick = () => send({ cmd: "spatial", mode: b.dataset.mode });
$("lap").oninput = (e) => send({ cmd: "spatial", lap_s: Number(e.target.value) });
$("dir").onclick = () => send({ cmd: "spatial", clockwise: !last?.spatial.settings.clockwise });
$("width").oninput = (e) => send({ cmd: "spatial", width_deg: Number(e.target.value) });
$("arrange").onclick = () => send({ cmd: "arrange" });
$("test").onclick = () => send({ cmd: "test" });

connect();
