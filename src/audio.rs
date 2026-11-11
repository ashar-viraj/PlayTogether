//! PipeWire side of the host.
//!
//! * A virtual sink (`Playtogether`) that becomes the default output, so every app plays into it.
//! * A 7.1 capture stream on that sink's monitor. Each cycle is stamped with its capture time
//!   `t`, turned into a 3D scene (see `spatial`), and handed to receivers as "play this at `t + D`".
//! * A playback stream to the real speakers that plays this PC's share on the same schedule,
//!   so it stays in step with the rest of the room.

use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use axum::body::Bytes;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::pod::Pod;
use tokio::sync::broadcast;

use crate::clock;
use crate::spatial::{self, CAPTURE_CHANNELS, Member, SceneBuilder};
use crate::state::{AudioCtl, CHANNELS, FRAME_BYTES, QUANTUM, RATE};

pub const SINK_NAME: &str = "Playtogether";

/// Packet header: kind u8, channels u8, frames u16, seq u32, play-at time f64 (host µs).
pub const HEADER_BYTES: usize = 16;

/// If the expected position drifts further than this from where it should be, jump instead of
/// gently speeding up or slowing down.
const RESYNC_US: f64 = 15_000.0;
/// Max playback speed correction (±0.3%, inaudible) used to absorb clock drift.
const MAX_RATE_ADJ: f64 = 0.003;

/// One captured cycle, shared by every receiver's sender task.
pub struct Chunk {
    pub seq: u32,
    pub t_play_us: f64,
    pub frames: usize,
    /// Ready-made stereo packet for when 3D is off.
    pub stereo_packet: Bytes,
    /// Present when 3D is on: each receiver mixes its own share from it.
    pub scene: Option<spatial::Scene>,
}

/// Builds a wire packet from interleaved samples in -1..1.
pub fn packet(seq: u32, t_play_us: f64, channels: u8, samples: &[f32]) -> Bytes {
    let frames = samples.len() / channels as usize;
    let mut pkt = Vec::with_capacity(HEADER_BYTES + samples.len() * 2);
    pkt.push(1);
    pkt.push(channels);
    pkt.extend_from_slice(&(frames as u16).to_le_bytes());
    pkt.extend_from_slice(&seq.to_le_bytes());
    pkt.extend_from_slice(&t_play_us.to_le_bytes());
    for &x in samples {
        pkt.extend_from_slice(&to_i16(x).to_le_bytes());
    }
    Bytes::from(pkt)
}

fn to_i16(x: f32) -> i16 {
    (x * 32767.0).clamp(-32768.0, 32767.0) as i16
}

pub struct AudioHandle {
    thread: JoinHandle<()>,
    quit: pw::channel::Sender<()>,
}

impl AudioHandle {
    pub fn stop(self) {
        let _ = self.quit.send(());
        let _ = self.thread.join();
    }
}

pub fn start(
    ctl: Arc<AudioCtl>,
    spatial: Arc<spatial::Shared>,
    audio_tx: broadcast::Sender<Arc<Chunk>>,
    pc_output: String,
) -> anyhow::Result<AudioHandle> {
    let (quit_tx, quit_rx) = pw::channel::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let thread = std::thread::Builder::new()
        .name("pipewire".into())
        .spawn(move || {
            if let Err(e) = run(ctl, spatial, audio_tx, pc_output, quit_rx, &ready_tx) {
                let _ = ready_tx.send(Err(format!("{e:#}")));
            }
        })?;
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(AudioHandle { thread, quit: quit_tx }),
        Ok(Err(e)) => bail!("PipeWire setup failed: {e}"),
        Err(_) => bail!("PipeWire setup timed out (is PipeWire running?)"),
    }
}

fn run(
    ctl: Arc<AudioCtl>,
    spatial: Arc<spatial::Shared>,
    audio_tx: broadcast::Sender<Arc<Chunk>>,
    pc_output: String,
    quit_rx: pw::channel::Receiver<()>,
    ready_tx: &mpsc::Sender<Result<(), String>>,
) -> anyhow::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None).context("can't connect to PipeWire")?;

    let _quit = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });

    // The sink is owned by this connection, so it disappears with us even if we crash.
    let _sink: pw::node::Node = core
        .create_object(
            "adapter",
            &properties! {
                "factory.name" => "support.null-audio-sink",
                "node.name" => SINK_NAME,
                "node.description" => "PlayTogether (whole room)",
                "media.class" => "Audio/Sink",
                // 7.1 so movie players can send real surround; stereo apps just use FL/FR.
                "audio.position" => "FL,FR,FC,LFE,RL,RR,SL,SR",
                // Let the normal volume keys act as the room's master volume.
                "monitor.channel-volumes" => "true",
                "object.linger" => "false",
            },
        )
        .context("can't create the virtual sink")?;
    roundtrip(&mainloop, &core);

    let ring = Arc::new(Mutex::new(Ring::new()));
    let latency = format!("{QUANTUM}/{RATE}");

    let capture = pw::stream::StreamBox::new(
        &core,
        "PlayTogether capture",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::NODE_NAME => "Playtogether.capture",
            *pw::keys::TARGET_OBJECT => SINK_NAME,
            *pw::keys::STREAM_CAPTURE_SINK => "true",
            *pw::keys::NODE_LATENCY => latency.as_str(),
            "node.dont-reconnect" => "true",
            "node.dont-fallback" => "true",
        },
    )?;
    let _capture_listener = capture
        .add_local_listener_with_user_data(Capture {
            ctl: ctl.clone(),
            tx: audio_tx,
            ring: ring.clone(),
            scene: SceneBuilder::new(spatial),
            seq: 0,
            next_t_us: None,
        })
        .process(|stream, st| st.process(stream))
        .register()?;
    let capture_format = format_pod(spa::param::audio::AudioFormat::F32LE, &[
        spa::sys::SPA_AUDIO_CHANNEL_FL,
        spa::sys::SPA_AUDIO_CHANNEL_FR,
        spa::sys::SPA_AUDIO_CHANNEL_FC,
        spa::sys::SPA_AUDIO_CHANNEL_LFE,
        spa::sys::SPA_AUDIO_CHANNEL_RL,
        spa::sys::SPA_AUDIO_CHANNEL_RR,
        spa::sys::SPA_AUDIO_CHANNEL_SL,
        spa::sys::SPA_AUDIO_CHANNEL_SR,
    ]);
    capture.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut [Pod::from_bytes(&capture_format).ok_or_else(|| anyhow!("bad format pod"))?],
    )?;

    let playback = pw::stream::StreamBox::new(
        &core,
        "PlayTogether (this PC)",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::NODE_NAME => "Playtogether.pc-output",
            *pw::keys::TARGET_OBJECT => pc_output.as_str(),
            *pw::keys::NODE_LATENCY => latency.as_str(),
            // Never fall back to the default sink: that's our own virtual sink, and playing
            // into it would loop our output back into the capture.
            "node.dont-reconnect" => "true",
            "node.dont-fallback" => "true",
        },
    )?;
    let _playback_listener = playback
        .add_local_listener_with_user_data(Playback { ctl: ctl.clone(), ring, cursor: None })
        .process(|stream, st| st.process(stream))
        .register()?;
    let playback_format = format_pod(
        spa::param::audio::AudioFormat::S16LE,
        &[spa::sys::SPA_AUDIO_CHANNEL_FL, spa::sys::SPA_AUDIO_CHANNEL_FR],
    );
    playback.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut [Pod::from_bytes(&playback_format).ok_or_else(|| anyhow!("bad format pod"))?],
    )?;

    let _ = ready_tx.send(Ok(()));
    mainloop.run();

    let _ = capture.disconnect();
    let _ = playback.disconnect();
    Ok(())
}

/// 48 kHz in the given sample format and channel layout; PipeWire converts to it.
fn format_pod(format: spa::param::audio::AudioFormat, channels: &[u32]) -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(format);
    info.set_rate(RATE);
    info.set_channels(channels.len() as u32);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[..channels.len()].copy_from_slice(channels);
    info.set_position(position);
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .expect("serializing a fixed audio format can't fail")
    .0
    .into_inner()
}

/// Wait until the server has processed everything we sent so far.
fn roundtrip(mainloop: &pw::main_loop::MainLoopRc, core: &pw::core::CoreRc) {
    let done = Rc::new(std::cell::Cell::new(false));
    let Ok(pending) = core.sync(0) else { return };
    let _listener = core
        .add_listener_local()
        .done({
            let done = done.clone();
            let mainloop = mainloop.clone();
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    done.set(true);
                    mainloop.quit();
                }
            }
        })
        .register();
    while !done.get() {
        mainloop.run();
    }
}

struct Capture {
    ctl: Arc<AudioCtl>,
    tx: broadcast::Sender<Arc<Chunk>>,
    ring: Arc<Mutex<Ring>>,
    scene: SceneBuilder,
    seq: u32,
    /// Predicted capture time of the next frame. Following the sample count instead of raw
    /// wakeup times keeps packet timestamps free of scheduling jitter.
    next_t_us: Option<f64>,
}

impl Capture {
    fn process(&mut self, stream: &pw::stream::Stream) {
        let Some(mut buffer) = stream.dequeue_buffer() else { return };
        let datas = buffer.datas_mut();
        let Some(data) = datas.first_mut() else { return };
        let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
        let Some(raw) = data.data() else { return };
        let end = (offset + size).min(raw.len());
        let bytes = &raw[offset.min(end)..end];
        let frames = bytes.len() / (4 * CAPTURE_CHANNELS);
        if frames == 0 {
            return;
        }
        let pcm: Vec<f32> = bytes[..frames * 4 * CAPTURE_CHANNELS]
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();

        let now_us = match stream.time() {
            Ok(t) if t.now() > 0 => t.now() as f64 / 1000.0,
            _ => clock::now_us() as f64,
        };
        let t_cap = match self.next_t_us {
            Some(pred) if (pred - now_us).abs() < RESYNC_US => pred + (now_us - pred) * 0.01,
            _ => now_us,
        };
        self.next_t_us = Some(t_cap + frames as f64 * 1e6 / RATE as f64);
        let t_play = t_cap + self.ctl.delay_us.load(Relaxed) as f64;

        let processed = self.scene.process(&pcm, t_play);
        let peak = processed.stereo.iter().fold(0.0f32, |p, x| p.max(x.abs()));
        self.ctl.report_peak(peak);
        self.ctl.capturing.store(true, Relaxed);

        // This PC's own share: the plain stereo mix, or its mono slice of the 3D scene.
        let pc_frames: Vec<[i16; 2]> = match &processed.scene {
            None => processed.stereo.chunks_exact(2).map(|s| [to_i16(s[0]), to_i16(s[1])]).collect(),
            Some(scene) => spatial::mix(scene, &processed.layout, Member::Pc, frames)
                .into_iter()
                .map(|x| [to_i16(x); 2])
                .collect(),
        };
        if let Ok(mut ring) = self.ring.lock() {
            ring.push(t_play, &pc_frames);
        }

        let stereo_packet = match processed.scene {
            None => packet(self.seq, t_play, CHANNELS as u8, &processed.stereo),
            Some(_) => Bytes::new(),
        };
        // No receivers is fine; the PC output reads from the ring above.
        let _ = self.tx.send(Arc::new(Chunk {
            seq: self.seq,
            t_play_us: t_play,
            frames,
            stereo_packet,
            scene: processed.scene,
        }));
        self.seq = self.seq.wrapping_add(1);
    }
}

struct Playback {
    ctl: Arc<AudioCtl>,
    ring: Arc<Mutex<Ring>>,
    /// Fractional ring frame index of the next frame to output.
    cursor: Option<f64>,
}

impl Playback {
    fn process(&mut self, stream: &pw::stream::Stream) {
        let Some(mut buffer) = stream.dequeue_buffer() else { return };
        let requested = buffer.requested() as usize;
        let datas = buffer.datas_mut();
        let Some(data) = datas.first_mut() else { return };
        let Some(out) = data.data() else { return };
        let max_frames = out.len() / FRAME_BYTES;
        let n = if requested > 0 { requested.min(max_frames) } else { max_frames.min(QUANTUM as usize) };

        // When will the first frame we write now come out of the speaker?
        let (heard_us, out_latency_us) = match stream.time() {
            Ok(t) if t.now() > 0 && t.rate().denom > 0 => {
                let ticks = (t.delay() + t.buffered() as i64) as f64;
                let lat = ticks * t.rate().num as f64 * 1e6 / t.rate().denom as f64;
                (t.now() as f64 / 1000.0 + lat, lat)
            }
            _ => (clock::now_us() as f64, 0.0),
        };
        self.ctl.pc_out_latency_us.store(out_latency_us as i64, Relaxed);
        let want_t = heard_us + self.ctl.pc_offset_us.load(Relaxed) as f64;
        let gain = if self.ctl.pc_muted.load(Relaxed) { 0.0 } else { self.ctl.pc_volume() };

        let ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        let out = &mut out[..n * FRAME_BYTES];
        match ring.frame_at(want_t) {
            None => {
                out.fill(0);
                self.cursor = None;
            }
            Some(want_f) => {
                let resync_frames = RESYNC_US * RATE as f64 / 1e6;
                let (mut c, ratio) = match self.cursor {
                    Some(c) if (want_f - c).abs() < resync_frames => {
                        // Behind schedule → play slightly faster, ahead → slightly slower.
                        let err_s = (want_f - c) / RATE as f64;
                        (c, 1.0 + err_s.clamp(-MAX_RATE_ADJ, MAX_RATE_ADJ))
                    }
                    _ => (want_f, 1.0),
                };
                let synced = self.cursor.is_some();
                let mut starved = false;
                for frame in out.chunks_exact_mut(FRAME_BYTES) {
                    let (l, r) = match ring.sample(c) {
                        Some(s) => s,
                        None => {
                            starved |= c >= ring.newest_frame();
                            (0.0, 0.0)
                        }
                    };
                    frame[0..2].copy_from_slice(&((l * gain) as i16).to_le_bytes());
                    frame[2..4].copy_from_slice(&((r * gain) as i16).to_le_bytes());
                    c += ratio;
                }
                if starved && synced {
                    self.ctl.pc_underruns.fetch_add(1, Relaxed);
                }
                let slack_us = (ring.newest_frame() - c) * 1e6 / RATE as f64;
                self.ctl.pc_min_slack_us.fetch_min(slack_us as i64, Relaxed);
                self.cursor = Some(c);
            }
        }
        drop(ring);

        let chunk = data.chunk_mut();
        *chunk.offset_mut() = 0;
        *chunk.stride_mut() = FRAME_BYTES as i32;
        *chunk.size_mut() = (n * FRAME_BYTES) as u32;
    }
}

/// The last ~2.7 s of captured audio, with the play-at time of every packet, so the PC output
/// can find "the sample that should be heard at time T".
struct Ring {
    frames: Vec<[i16; 2]>,
    /// Total frames ever written; frame `f` lives at `f % len`.
    written: u64,
    /// (first frame index, play-at time in µs) for each packet still in the ring.
    segments: VecDeque<(u64, f64)>,
}

impl Ring {
    const LEN: usize = 1 << 17;

    fn new() -> Self {
        Self { frames: vec![[0, 0]; Self::LEN], written: 0, segments: VecDeque::new() }
    }

    fn push(&mut self, t_play_us: f64, frames: &[[i16; 2]]) {
        self.segments.push_back((self.written, t_play_us));
        for &f in frames {
            let idx = (self.written % Self::LEN as u64) as usize;
            self.frames[idx] = f;
            self.written += 1;
        }
        let oldest = self.written.saturating_sub(Self::LEN as u64);
        while self.segments.len() > 1 && self.segments[1].0 <= oldest {
            self.segments.pop_front();
        }
    }

    fn newest_frame(&self) -> f64 {
        self.written as f64
    }

    /// Frame index that should be heard at `t_us`. The newest packet wins when two overlap
    /// (that happens right after the room delay shrinks).
    fn frame_at(&self, t_us: f64) -> Option<f64> {
        let &(f, t) = self.segments.iter().rev().find(|&&(_, t)| t <= t_us)?;
        Some(f as f64 + (t_us - t) * RATE as f64 / 1e6)
    }

    /// Linearly interpolated sample at a fractional frame index, if it's still in the ring.
    fn sample(&self, f: f64) -> Option<(f32, f32)> {
        if f < 0.0 {
            return None;
        }
        let i = f.floor() as u64;
        if i + 1 >= self.written || i < self.written.saturating_sub(Self::LEN as u64 - 1) {
            return None;
        }
        let frac = (f - i as f64) as f32;
        let a = self.frames[(i % Self::LEN as u64) as usize];
        let b = self.frames[((i + 1) % Self::LEN as u64) as usize];
        let lerp = |x: i16, y: i16| x as f32 + (y as f32 - x as f32) * frac;
        Some((lerp(a[0], b[0]), lerp(a[1], b[1])))
    }
}
