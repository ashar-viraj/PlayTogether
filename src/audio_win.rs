//! WASAPI side of the host: the Windows twin of `audio.rs` (PipeWire), same method throughout.
//!
//! * The virtual sink is the VB-Audio Virtual Cable, made the default output by `sinkctl`, so
//!   every app plays into it. Its recording end is captured here. Each WASAPI packet carries the
//!   QPC time it was captured (the Windows counterpart of PipeWire's graph time), which is our
//!   host clock too. Cut into 256-frame cycles, each is turned into a 3D scene (see `spatial`)
//!   and handed to receivers as "play this at `t + D`".
//! * A playback stream to the real speakers plays this PC's share on the same schedule. When a
//!   sample will be heard comes from `IAudioClock::GetPosition`, the stream position currently
//!   at the speaker, so it includes the device's own delay, like PipeWire's `delay + buffered`.
//! * Format and rate conversion are left to WASAPI (`AUTOCONVERTPCM`), as PipeWire does them.
//!
//! Without the cable, the real output is captured with WASAPI loopback and the PC plays
//! directly (not delayed with the room).

use std::collections::VecDeque;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context as _, bail};
use axum::body::Bytes;
use tokio::sync::broadcast;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient, IAudioClient,
    IAudioClock, IAudioRenderClient, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Threading::{AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject};
use windows::core::{GUID, PCWSTR, w};

use crate::clock;
use crate::sinkctl;
use crate::spatial::{self, CAPTURE_CHANNELS, Member, SceneBuilder};
use crate::state::{AudioCtl, CHANNELS, QUANTUM, RATE};

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
    threads: Vec<JoinHandle<()>>,
    quit: Arc<AtomicBool>,
}

impl AudioHandle {
    pub fn stop(self) {
        self.quit.store(true, Relaxed);
        for t in self.threads {
            let _ = t.join();
        }
    }
}

/// `capture_from`: the cable's recording end (or, without the cable, an output to loopback).
/// `pc_play`: the real speakers that play this PC's share on the room schedule.
pub fn start(
    ctl: Arc<AudioCtl>,
    spatial: Arc<spatial::Shared>,
    audio_tx: broadcast::Sender<Arc<Chunk>>,
    capture_from: String,
    pc_play: Option<String>,
) -> anyhow::Result<AudioHandle> {
    let quit = Arc::new(AtomicBool::new(false));
    let ring = Arc::new(Mutex::new(Ring::new()));
    let mut threads = Vec::new();

    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    {
        let capture = Capture {
            ctl: ctl.clone(),
            tx: audio_tx,
            ring: ring.clone(),
            scene: SceneBuilder::new(spatial),
            seq: 0,
            next_t_us: None,
            pending: Vec::new(),
            pending_t_us: 0.0,
        };
        let (quit, ready) = (quit.clone(), ready_tx.clone());
        threads.push(std::thread::Builder::new().name("capture".into()).spawn(move || {
            if let Err(e) = capture_thread(&capture_from, capture, &quit, &ready) {
                let _ = ready.send(Err(format!("capture: {e:#}")));
            }
        })?);
    }
    if let Some(device) = pc_play {
        let playback = Playback { ctl, ring, cursor: None };
        let (quit, ready) = (quit.clone(), ready_tx.clone());
        threads.push(std::thread::Builder::new().name("pc-output".into()).spawn(move || {
            if let Err(e) = playback_thread(&device, playback, &quit, &ready) {
                let _ = ready.send(Err(format!("PC output: {e:#}")));
            }
        })?);
    }
    drop(ready_tx);

    let handle = AudioHandle { threads, quit };
    for _ in 0..handle.threads.len() {
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                handle.stop();
                bail!("audio setup failed: {e}");
            }
            Err(_) => {
                handle.stop();
                bail!("audio setup timed out");
            }
        }
    }
    Ok(handle)
}

/// COM, an auto-reset event for WASAPI to signal, and "Pro Audio" scheduling for this thread
/// (the counterpart of PipeWire's realtime data thread).
fn audio_thread_setup() -> anyhow::Result<HANDLE> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let mut task = 0u32;
        let _ = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task);
        Ok(CreateEventW(None, false, false, PCWSTR::null())?)
    }
}

const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const SPEAKER_FRONT_LEFT: u32 = 0x1;
const SPEAKER_FRONT_RIGHT: u32 = 0x2;
const SPEAKER_FRONT_CENTER: u32 = 0x4;

/// 48 kHz float in the given channel layout; WASAPI converts to it (like PipeWire does).
fn float_format(channels: u16, mask: u32) -> WAVEFORMATEXTENSIBLE {
    let block = channels * 4;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE,
            nChannels: channels,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * block as u32,
            nBlockAlign: block,
            wBitsPerSample: 32,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
        dwChannelMask: mask,
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

/// The device's own channel count and speaker mask.
fn device_layout(client: &IAudioClient) -> anyhow::Result<(u16, u32)> {
    unsafe {
        let mix = client.GetMixFormat()?;
        let fmt = std::ptr::read_unaligned(mix);
        let channels = fmt.nChannels;
        let mask = if fmt.wFormatTag == WAVE_FORMAT_EXTENSIBLE {
            std::ptr::read_unaligned(mix as *const WAVEFORMATEXTENSIBLE).dwChannelMask
        } else {
            0
        };
        CoTaskMemFree(Some(mix as *const _));
        let mask = match (mask, channels) {
            (0, 1) => SPEAKER_FRONT_CENTER,
            (0, _) => SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
            (m, _) => m,
        };
        Ok((channels, mask))
    }
}

/// For each device channel (in WASAPI order, i.e. ascending speaker-mask bits), where it goes
/// in our FL,FR,FC,LFE,RL,RR,SL,SR layout.
fn channel_slots(mask: u32, channels: u16) -> Vec<Option<usize>> {
    let slot_of = |bit: u32| match bit {
        0x1 => Some(0),   // front left
        0x2 => Some(1),   // front right
        0x4 => Some(2),   // front centre
        0x8 => Some(3),   // LFE
        0x10 => Some(4),  // back left
        0x20 => Some(5),  // back right
        0x200 => Some(6), // side left
        0x400 => Some(7), // side right
        _ => None,
    };
    let mut slots: Vec<Option<usize>> = (0..32).map(|b| 1u32 << b).filter(|b| mask & b != 0).map(slot_of).collect();
    slots.resize(channels as usize, None);
    slots
}

fn capture_thread(
    device: &str,
    mut cap: Capture,
    quit: &AtomicBool,
    ready: &mpsc::Sender<Result<(), String>>,
) -> anyhow::Result<()> {
    let event = audio_thread_setup()?;
    // The cable's recording end is captured as-is; an output device is captured with loopback.
    let loopback = !sinkctl::is_recording_device(device);
    let dev = sinkctl::open_device(loopback, device)?;
    unsafe {
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).context("can't open the capture device")?;
        let (channels, mask) = device_layout(&client)?;
        let fmt = float_format(channels, mask);
        let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        // 100 ms of buffer; WASAPI still wakes us every device period.
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 1_000_000, 0, &fmt as *const _ as *const WAVEFORMATEX, None)
            .context("can't start capturing")?;
        client.SetEventHandle(event)?;
        let cc: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        let _ = ready.send(Ok(()));

        let slots = channel_slots(mask, channels);
        let ch = channels as usize;
        while !quit.load(Relaxed) {
            WaitForSingleObject(event, 50);
            while cc.GetNextPacketSize()? > 0 {
                let mut data: *mut u8 = std::ptr::null_mut();
                let (mut frames, mut bufflags, mut qpc) = (0u32, 0u32, 0u64);
                cc.GetBuffer(&mut data, &mut frames, &mut bufflags, None, Some(&mut qpc))?;
                let n = frames as usize;
                let mut pcm = vec![0.0f32; n * CAPTURE_CHANNELS];
                const SILENT: u32 = 0x2;
                if bufflags & SILENT == 0 && !data.is_null() {
                    let src = std::slice::from_raw_parts(data as *const f32, n * ch);
                    for (f, out) in src.chunks_exact(ch).zip(pcm.chunks_exact_mut(CAPTURE_CHANNELS)) {
                        if ch == 1 {
                            out[0] = f[0];
                            out[1] = f[0];
                            continue;
                        }
                        for (c, &x) in f.iter().enumerate() {
                            if let Some(s) = slots[c] {
                                out[s] = x;
                            }
                        }
                    }
                }
                cc.ReleaseBuffer(frames)?;
                // QPC time of this packet's first frame: when it was captured.
                let t_us = if qpc > 0 { clock::qpc_100ns_to_us(qpc) } else { clock::now_us() as f64 };
                cap.feed(&pcm, t_us);
            }
        }
        let _ = client.Stop();
        let _ = CloseHandle(event);
    }
    Ok(())
}

fn playback_thread(
    device: &str,
    mut pb: Playback,
    quit: &AtomicBool,
    ready: &mpsc::Sender<Result<(), String>>,
) -> anyhow::Result<()> {
    let event = audio_thread_setup()?;
    let dev = sinkctl::open_device(true, device)?;
    unsafe {
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).context("can't open the PC output")?;
        let fmt = float_format(CHANNELS as u16, SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT);
        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        // Smallest buffer WASAPI allows in shared mode (two device periods).
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, &fmt as *const _ as *const WAVEFORMATEX, None)
            .context("can't start the PC output")?;
        client.SetEventHandle(event)?;
        let size = client.GetBufferSize()?;
        let rc: IAudioRenderClient = client.GetService()?;
        let audio_clock: IAudioClock = client.GetService()?;
        let freq = audio_clock.GetFrequency()?.max(1) as f64;
        client.Start()?;
        let _ = ready.send(Ok(()));

        // Frames handed to WASAPI so far, at our 48 kHz stream rate.
        let mut written: u64 = 0;
        while !quit.load(Relaxed) {
            WaitForSingleObject(event, 50);
            let padding = client.GetCurrentPadding()?;
            let n = size.saturating_sub(padding);
            if n == 0 {
                continue;
            }
            // When will the first frame we write now come out of the speaker? The device
            // position is the sample at the speaker right now (at time `qpc`); everything we
            // wrote after it is still ahead of the speaker.
            let now = clock::now_us() as f64;
            let (mut pos, mut qpc) = (0u64, 0u64);
            let heard_us = match audio_clock.GetPosition(&mut pos, Some(&mut qpc)) {
                Ok(()) if qpc > 0 => {
                    let ahead_s = written as f64 / RATE as f64 - pos as f64 / freq;
                    clock::qpc_100ns_to_us(qpc) + ahead_s * 1e6
                }
                _ => now + padding as f64 * 1e6 / RATE as f64,
            };
            let buf = rc.GetBuffer(n)?;
            let out = std::slice::from_raw_parts_mut(buf as *mut f32, n as usize * CHANNELS);
            pb.process(out, heard_us, heard_us - now);
            rc.ReleaseBuffer(n, 0)?;
            written += n as u64;
        }
        let _ = client.Stop();
        let _ = CloseHandle(event);
    }
    Ok(())
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
    /// Captured 7.1 frames not yet sent, and the capture time of the first of them. WASAPI
    /// packets are cut into the same 256-frame cycles PipeWire runs at.
    pending: Vec<f32>,
    pending_t_us: f64,
}

impl Capture {
    fn feed(&mut self, pcm: &[f32], t_us: f64) {
        if self.pending.is_empty() {
            self.pending_t_us = t_us;
        }
        self.pending.extend_from_slice(pcm);
        let cycle = QUANTUM as usize * CAPTURE_CHANNELS;
        while self.pending.len() >= cycle {
            let chunk: Vec<f32> = self.pending.drain(..cycle).collect();
            let t = self.pending_t_us;
            self.pending_t_us += QUANTUM as f64 * 1e6 / RATE as f64;
            self.process(&chunk, t);
        }
    }

    /// Same as the PipeWire capture's `process`, with `now_us` = the cycle's capture time.
    fn process(&mut self, pcm: &[f32], now_us: f64) {
        let frames = pcm.len() / CAPTURE_CHANNELS;
        let t_cap = match self.next_t_us {
            Some(pred) if (pred - now_us).abs() < RESYNC_US => pred + (now_us - pred) * 0.01,
            _ => now_us,
        };
        self.next_t_us = Some(t_cap + frames as f64 * 1e6 / RATE as f64);
        let t_play = t_cap + self.ctl.delay_us.load(Relaxed) as f64;

        let processed = self.scene.process(pcm, t_play);
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
    /// Same as the PipeWire playback's `process`: fill `out` (stereo f32) with the samples that
    /// should be heard from `heard_us` on.
    fn process(&mut self, out: &mut [f32], heard_us: f64, out_latency_us: f64) {
        self.ctl.pc_out_latency_us.store(out_latency_us as i64, Relaxed);
        let want_t = heard_us + self.ctl.pc_offset_us.load(Relaxed) as f64;
        let gain = if self.ctl.pc_muted.load(Relaxed) { 0.0 } else { self.ctl.pc_volume() } / 32768.0;

        let ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        match ring.frame_at(want_t) {
            None => {
                out.fill(0.0);
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
                for frame in out.chunks_exact_mut(CHANNELS) {
                    let (l, r) = match ring.sample(c) {
                        Some(s) => s,
                        None => {
                            starved |= c >= ring.newest_frame();
                            (0.0, 0.0)
                        }
                    };
                    frame[0] = l * gain;
                    frame[1] = r * gain;
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
