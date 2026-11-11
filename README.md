# PlayTogether

Play whatever this PC is playing (YouTube, Spotify, VLC, games, anything) on every phone and
PC in the room, **in sync**. Runs on **Linux** and **Windows**. Receivers just open a web page, with no app to install.

## Install (one time)

| | |
| --- | --- |
| **Windows** | Double-click **`install.bat`**, or in PowerShell: `irm https://raw.githubusercontent.com/ashar-viraj/PlayTogether/main/install.ps1 \| iex` |
| **Linux** | `./install.sh`, or: `curl -fsSL https://raw.githubusercontent.com/ashar-viraj/PlayTogether/main/install.sh \| bash` |

The installer figures out the rest: it downloads the prebuilt app from the latest
[release](https://github.com/ashar-viraj/PlayTogether/releases) (or builds it from source, installing Rust and
build tools if needed) and adds **PlayTogether** and **Stop PlayTogether** shortcuts (Start menu + desktop on
Windows, app menu on Linux; on Linux also the `playtogether` / `playtogether-stop` commands).
On Windows it also installs the free [VB-Audio Virtual Cable](https://vb-audio.com/Cable/) (Windows has no
built-in virtual sink; see below) and allows PlayTogether through the firewall, with one admin prompt.

Start it, scan the QR code on the dashboard with your phones, and you're playing. Stop it with
**Stop PlayTogether** (or Ctrl+C in its window): your normal speakers come back.

**Releasing:** pushing a version tag (`git tag v0.2.0 && git push origin v0.2.0`) makes GitHub Actions build both
platforms and publish them as a release, which the installers then download.

## Windows

Same method as on Linux, with Windows' audio APIs:

| | Linux | Windows |
| --- | --- | --- |
| virtual sink every app plays into | PipeWire null sink (created on the fly) | VB-Audio Virtual Cable (made the default output while running) |
| capture timestamps / host clock | PipeWire graph time, `CLOCK_MONOTONIC` | WASAPI packet QPC time, `QueryPerformanceCounter` |
| when this PC's speaker really plays | PipeWire `delay + buffered` | `IAudioClock::GetPosition` |
| format/rate conversion | PipeWire | WASAPI `AUTOCONVERTPCM` |

Without VB-Cable it still works by capturing the speakers with WASAPI loopback, but then this PC plays straight
away, ahead of the phones by the room delay.

```
 apps ──▶ [PlayTogether virtual sink] ──monitor──▶ host (Rust) ──WebSocket──▶ phones / PCs (browser)
                                                       │
                                                       └──▶ this PC's real speakers (same schedule)
```

## Run it from source (Linux)

```bash
./scripts/bootstrap-deps.sh     # once: fetches PipeWire/clang headers into .deps (no sudo)
                                # or instead: sudo apt install libpipewire-0.3-dev libclang-dev pkg-config
cargo run --release
```

Then:

- **Dashboard** (on this PC): http://localhost:8090/host shows a QR code, the room delay, PC mute and per-device controls.
- **Receivers**: scan the QR code (or open `http://<this-pc-ip>:8090/`) on any device on the same Wi-Fi and tap **Join**.
- **Stop** with Ctrl+C. Your normal output device is restored. If it ever crashes, sound falls back to
  your speakers by itself, and your original default is restored on the next clean exit.

Options:

| flag | meaning |
| --- | --- |
| `--port 8090` | web port |
| `--delay-ms 80` | fixed room delay instead of automatic |
| `--mute-pc` | start with this PC silent (receivers only); also a toggle in the dashboard |
| `--pc-output <sink>` | which PipeWire sink this PC plays on (default: the current output) |

Requirements: Linux with PipeWire and WirePlumber (default on Ubuntu 22.10+, Fedora 34+). Receivers need
any modern browser.

### A device can't connect (page keeps loading)

1. Use the URL the terminal or dashboard shows right now. It follows Wi-Fi changes, so an address
   from an earlier network won't work.
2. Watch the terminal. When a device reaches this PC you'll see `<its ip> opened the page`.
   - **Nothing appears:** the network isn't letting devices talk to each other. Common causes:
     - The device is on mobile data, a different Wi-Fi, or a VPN (e.g. the 1.1.1.1/WARP app on the phone).
       Turn the VPN off.
     - The router has *AP/client isolation* turned on, or you're on a guest network. Campus and office Wi-Fi
       usually do this. Turn isolation off, or run everything on a phone or laptop hotspot.
     - A firewall on this PC: `sudo ufw allow 8090/tcp`.
   - **It appears, but no sound:** tap **Join** on the device and check its volume and silent switch.

## How sync works

"Zero delay" from the source isn't physically possible: Wi-Fi, the phone's audio stack and Bluetooth all add
time. What PlayTogether does instead is make **every device play each sample at the same moment**:

1. **Timestamps.** Each 5.3 ms chunk captured from the virtual sink is stamped with *play at T = capture time + D*,
   in the host's monotonic clock. D is the *room delay*.
2. **Clock sync.** Each page keeps estimating the offset between its clock and the host's (NTP-style
   ping/pong, using the lowest-round-trip recent sample). On the LAN this is accurate to well under 1 ms.
3. **Output-aware scheduling.** The page maps host time onto the moment sound actually leaves the device
   (`AudioContext.getOutputTimestamp`, so the device's output latency is accounted for) and schedules each
   chunk with Web Audio's sample-accurate `start(when)`.
4. **Drift correction.** Clocks drift by tens of ppm, so playback speed is nudged by up to ±0.3%
   (inaudible) to stay locked. Larger errors resync with a jump.
5. **This PC** plays the same timestamped stream through PipeWire to your real speakers, using PipeWire's
   reported device delay, so it lines up with the rest of the room. That's why the PC's own sound is
   delayed by D too.
6. **Automatic room delay.** Every device reports its *headroom* (how early its audio arrived, minus a
   safety margin). D rises immediately when any device runs short and drifts down after a calm
   stretch, ending as low as the slowest device allows. On this machine alone it settles around
   17 ms. With phones on Wi-Fi expect about 40–120 ms, depending on the phone and network.

Measured on the dev machine: a Chrome receiver and the PC speakers played within **about 2–3 ms** of each
other (±0.6 ms wobble), well below the roughly 10–15 ms where people start hearing two sources.

## 3D sound

Put the devices in a circle around you with the PC screen in front (12 o'clock), then pick a mode in the
dashboard's **3D sound** card:

| mode | what you hear |
| --- | --- |
| Off | every device plays the normal stereo mix |
| Surround | movies/games with **5.1/7.1** audio (VLC, mpv, games): each channel plays from its own direction. **Stereo** (YouTube, Spotify): left and right wrap around the front, and the room's ambience (the left−right difference, delayed and softened) comes from behind |
| Rotate | the music circles round you; lap time (2–30 s), direction and width are adjustable |
| Position | drag the glowing dot on the room map to put the music anywhere around you |

**Where to put each device.** The dashboard's room map and *Where to put each device* list show every seat
as a clock position, and each phone shows its own spot ("Your spot: 4 o'clock"). Automatic placement spaces
devices evenly with this PC at 12. It gives front-right and front-left (the seats that carry most of the
music) to the biggest speakers: computers first, then tablets, with phones at the sides and back. Drag a device
on the map to match where it really is; the layout is saved and kept when devices reconnect. **Speaker
test** chimes on each device clockwise from 12 while its screen flashes, so you can check the order;
**Identify** on a device card does it for one device.

For the best effect:
- every device the **same distance** from you (each extra metre is ~3 ms late and smears the effect),
- speakers facing you, around ear height, at similar volume (use the per-device volume),
- 4–8 devices. With 2 it's just stereo; with 3 the back is thin.

How it works: the virtual sink is 7.1, so movie players can hand over real surround channels (stereo apps only
fill front-left/right, which is how stereo is told apart from surround). Each 5 ms chunk becomes a few
virtual sources at angles around you, and every device plays its share with constant-power panning between
the two devices on either side of each source. Rotation and dot positions are fixed per chunk at capture
time, so all devices move the sound in lockstep. In 3D modes each device gets only its own mono mix (half the
bandwidth of stereo).

### Sync nudge

Some delay isn't visible to the browser, notably **Bluetooth** headphones/speakers (100–300 ms) and
some phones' audio hardware. If a device sounds behind the others, drag its **Sync nudge** right (or left if
it's ahead), on the device itself or from the dashboard. It's saved per device. Bluetooth devices will push
the automatic room delay up for everyone.

### Notes and limits

- YouTube video will be D ms ahead of the sound (unnoticeable below ~45 ms, noticeable above ~100 ms).
- Receivers keep playing with the screen off or in another app. Phones only keep media pages running in the
  background, so the page loops a silent `<audio>` element next to its Web Audio output; it shows up as
  "PlayTogether" on the lock screen. Pausing it there (or a call, or another app taking over audio)
  silences that device until it's played again or the page is tapped. If a device was frozen anyway, it
  resyncs when it wakes and drops the stale audio it missed instead of raising the room delay.
- The page is plain HTTP on the LAN. Opening the receiver page on the host PC itself is blocked, since
  it would play into the room sink and loop the audio back.
- Bandwidth is uncompressed 16-bit stereo, about 1.6 Mbit/s per receiver: fine for a room full of devices on
  normal Wi-Fi.

## Layout

| path | what |
| --- | --- |
| `src/audio.rs` | PipeWire: 7.1 virtual sink, capture + timestamps, ring buffer, PC playback with drift correction |
| `src/spatial.rs` | 3D: scenes (surround / spread / rotate / position / speaker test) and per-device panning |
| `src/room.rs` | seats around the circle: automatic placement, dragging, saved layout |
| `src/controller.rs` | automatic room delay |
| `src/server.rs` | HTTP + WebSockets (receivers, dashboard), QR code |
| `src/sinkctl.rs` | switching the default output to the virtual sink and back |
| `src/state.rs` | shared state, dashboard commands |
| `web/receiver.*` | the page phones open: clock sync, scheduling, nudge |
| `web/dashboard.*` | host dashboard |

Wire format: binary WebSocket frames, `u8 kind=1, u8 channels, u16 frames, u32 seq, f64 play_at_us`
(little-endian), then interleaved s16 PCM (2 channels normally, 1 = this device's 3D share). JSON text frames carry ping/pong, stats and settings.

For testing on one machine, the hidden `--dev-allow-local-receivers` flag lets a browser on the host receive
audio. Route that browser to another sink (e.g. `PULSE_SINK=...`) or it will feed back.
