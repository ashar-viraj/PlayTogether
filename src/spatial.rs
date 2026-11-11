//! 3D sound around a circle of devices.
//!
//! Each captured chunk becomes a *scene*: a few virtual sources, each at an angle around the
//! listener (0° = front = this PC, clockwise, so 90° is 3 o'clock). Every device then plays its
//! share of each source with constant-power panning between the two devices on either side of
//! that source. Moving sources carry their angle at the start and end of the chunk. Those angles
//! are fixed once at capture time, so every device moves the sound in lockstep.

use std::f32::consts::{FRAC_PI_2, PI};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::state::RATE;

/// The virtual sink is 7.1 so movie players can hand us real surround channels.
pub const CAPTURE_CHANNELS: usize = 8;
const FL: usize = 0;
const FR: usize = 1;
const FC: usize = 2;
const LFE: usize = 3;
const RL: usize = 4;
const RR: usize = 5;
const SL: usize = 6;
const SR: usize = 7;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Every device plays the normal stereo mix.
    #[default]
    Off,
    /// Real 5.1/7.1 channels at their angles, or stereo spread around the room.
    Surround,
    /// The music circles the room.
    Rotate,
    /// The music sits wherever the dashboard dot is.
    Position,
}

#[derive(Clone, Debug, Serialize)]
pub struct Settings {
    pub mode: Mode,
    /// Seconds per lap in Rotate mode.
    pub lap_s: f32,
    pub clockwise: bool,
    /// Angle between the left and right halves of the music in Rotate/Position modes.
    pub width_deg: f32,
    /// Where the music sits in Position mode.
    pub target_deg: f32,
    #[serde(skip)]
    pub test: Option<Arc<TestRun>>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { mode: Mode::Off, lap_s: 8.0, clockwise: true, width_deg: 60.0, target_deg: 0.0, test: None }
    }
}

/// Speaker test: a chime at each angle in turn, on the play-at timeline.
#[derive(Debug)]
pub struct TestRun {
    pub start_us: f64,
    pub step_us: f64,
    pub angles: Vec<f32>,
}

impl TestRun {
    pub fn end_us(&self) -> f64 {
        self.start_us + self.step_us * self.angles.len() as f64
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Member {
    Pc,
    Client(u64),
}

/// Devices currently playing and their angles, sorted by angle.
#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub members: Vec<(Member, f32)>,
}

impl Layout {
    pub fn new(mut members: Vec<(Member, f32)>) -> Self {
        members.sort_by(|a, b| a.1.total_cmp(&b.1));
        Self { members }
    }

    pub fn angle_of(&self, m: Member) -> Option<f32> {
        self.members.iter().find(|(x, _)| *x == m).map(|&(_, a)| a)
    }

    /// Share of a source at `src_deg` that member `m` plays: constant-power panning between
    /// the two neighbouring devices around the source, silence everywhere else.
    pub fn gain(&self, m: Member, src_deg: f32) -> f32 {
        let n = self.members.len();
        match n {
            0 => return 0.0,
            1 => return if self.members[0].0 == m { 1.0 } else { 0.0 },
            _ => {}
        }
        for i in 0..n {
            let (mi, a) = self.members[i];
            let (mj, b) = self.members[(i + 1) % n];
            let span = norm(b - a);
            let offset = norm(src_deg - a);
            if span > 0.0 && offset <= span {
                let t = offset / span;
                return if m == mi {
                    (t * FRAC_PI_2).cos()
                } else if m == mj {
                    (t * FRAC_PI_2).sin()
                } else {
                    0.0
                };
            }
        }
        // Everyone stacked on one spot.
        1.0 / (n as f32).sqrt()
    }
}

pub fn norm(deg: f32) -> f32 {
    deg.rem_euclid(360.0)
}

pub struct Source {
    /// Angle at the first and last frame of the chunk.
    pub a0: f32,
    pub a1: f32,
    pub gain: f32,
    pub pcm: Vec<f32>,
}

pub struct Scene {
    pub sources: Vec<Source>,
    /// Bass channel, sent to every device. Empty when there's none.
    pub lfe: Vec<f32>,
}

/// One device's mono share of a scene.
pub fn mix(scene: &Scene, layout: &Layout, me: Member, frames: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; frames];
    for s in &scene.sources {
        let g0 = layout.gain(me, s.a0) * s.gain;
        let g1 = layout.gain(me, s.a1) * s.gain;
        if g0 == 0.0 && g1 == 0.0 {
            continue;
        }
        // Ramp the gain across the chunk so moving sources don't step.
        let step = (g1 - g0) / frames as f32;
        for (i, (o, x)) in out.iter_mut().zip(&s.pcm).enumerate() {
            *o += (g0 + step * i as f32) * x;
        }
    }
    if layout.angle_of(me).is_some() {
        for (o, x) in out.iter_mut().zip(&scene.lfe) {
            *o += 0.5 * x;
        }
    }
    out
}

/// Settings, layout and live state shared by the dashboard, the audio thread and the
/// per-receiver mixers.
#[derive(Default)]
pub struct Shared {
    pub settings: Mutex<Settings>,
    pub layout: Mutex<Arc<Layout>>,
    theta: AtomicU32,
    pub multichannel: AtomicBool,
}

impl Shared {
    pub fn layout(&self) -> Arc<Layout> {
        self.layout.lock().unwrap().clone()
    }

    pub fn theta(&self) -> f32 {
        f32::from_bits(self.theta.load(Relaxed))
    }
}

/// Lives in the capture callback; turns raw 7.1 chunks into scenes.
pub struct SceneBuilder {
    shared: Arc<Shared>,
    settings: Settings,
    layout: Arc<Layout>,
    theta: f32,
    side: Vec<f32>,
    side_pos: usize,
    lowpass: [f32; 2],
    multichannel_until_us: f64,
}

/// What the capture callback needs from one chunk.
pub struct Processed {
    /// Interleaved stereo downmix (for Off mode and the level meter).
    pub stereo: Vec<f32>,
    pub scene: Option<Scene>,
    pub layout: Arc<Layout>,
}

const SIDE_LEN: usize = 4096;
const AMBIENCE_DELAY_L: usize = RATE as usize * 12 / 1000;
const AMBIENCE_DELAY_R: usize = RATE as usize * 17 / 1000;
/// One-pole low-pass at ~6 kHz for the ambience channels.
const AMBIENCE_LP: f32 = 0.544;

impl SceneBuilder {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            settings: Settings::default(),
            layout: Arc::default(),
            theta: 0.0,
            side: vec![0.0; SIDE_LEN],
            side_pos: 0,
            lowpass: [0.0; 2],
            multichannel_until_us: 0.0,
        }
    }

    /// `pcm` is interleaved 7.1 f32; `t_play` is when its first frame should be heard.
    pub fn process(&mut self, pcm: &[f32], t_play: f64) -> Processed {
        // Never block the realtime thread: if someone holds a lock, reuse last chunk's copy.
        if let Ok(s) = self.shared.settings.try_lock() {
            self.settings = s.clone();
        }
        if let Ok(l) = self.shared.layout.try_lock() {
            self.layout = l.clone();
        }

        let frames = pcm.len() / CAPTURE_CHANNELS;
        let chan = |c: usize| -> Vec<f32> { (0..frames).map(|i| pcm[i * CAPTURE_CHANNELS + c]).collect() };

        let surround_active = pcm
            .chunks_exact(CAPTURE_CHANNELS)
            .any(|f| [FC, LFE, RL, RR, SL, SR].iter().any(|&c| f[c].abs() > 1e-4));
        if surround_active {
            self.multichannel_until_us = t_play + 2e6;
        }
        let multichannel = t_play < self.multichannel_until_us;
        self.shared.multichannel.store(multichannel, Relaxed);

        // ITU-style downmix; for plain stereo content this is exactly FL/FR.
        let mut left = Vec::with_capacity(frames);
        let mut right = Vec::with_capacity(frames);
        let mut stereo = Vec::with_capacity(frames * 2);
        for f in pcm.chunks_exact(CAPTURE_CHANNELS) {
            let l = f[FL] + 0.5 * (f[FC] + f[SL] + f[RL]) + 0.35 * f[LFE];
            let r = f[FR] + 0.5 * (f[FC] + f[SR] + f[RR]) + 0.35 * f[LFE];
            left.push(l);
            right.push(r);
            stereo.push(l);
            stereo.push(r);
        }

        // Keep the stereo "side" history running so spread mode can delay it.
        let mut amb_l = Vec::with_capacity(frames);
        let mut amb_r = Vec::with_capacity(frames);
        for (l, r) in left.iter().zip(&right) {
            self.side[self.side_pos] = (l - r) * 0.5;
            let dl = self.side[(self.side_pos + SIDE_LEN - AMBIENCE_DELAY_L) % SIDE_LEN];
            let dr = -self.side[(self.side_pos + SIDE_LEN - AMBIENCE_DELAY_R) % SIDE_LEN];
            self.side_pos = (self.side_pos + 1) % SIDE_LEN;
            self.lowpass[0] += AMBIENCE_LP * (dl - self.lowpass[0]);
            self.lowpass[1] += AMBIENCE_LP * (dr - self.lowpass[1]);
            amb_l.push(self.lowpass[0]);
            amb_r.push(self.lowpass[1]);
        }

        let dt = frames as f32 / RATE as f32;
        let theta0 = self.theta;
        match self.settings.mode {
            Mode::Rotate => {
                let dir = if self.settings.clockwise { 1.0 } else { -1.0 };
                self.theta = norm(self.theta + dir * 360.0 * dt / self.settings.lap_s.max(0.5));
            }
            Mode::Position => {
                // Glide towards the dot instead of jumping, max 540°/s along the short way round.
                let diff = norm(self.settings.target_deg - self.theta + 180.0) - 180.0;
                let max = 540.0 * dt;
                self.theta = norm(self.theta + diff.clamp(-max, max));
            }
            _ => {}
        }
        let theta1 = self.theta;
        self.shared.theta.store(self.theta.to_bits(), Relaxed);

        let scene = if let Some(test) = self.settings.test.clone().filter(|t| t_play < t.end_us()) {
            (t_play + dt as f64 * 1e6 > test.start_us).then(|| test_scene(&test, t_play, frames))
        } else {
            None
        };
        let scene = scene.or_else(|| {
            let src = |a: f32, gain: f32, pcm: Vec<f32>| Source { a0: a, a1: a, gain, pcm };
            match self.settings.mode {
                Mode::Off => None,
                Mode::Surround if multichannel => Some(Scene {
                    sources: vec![
                        src(-35.0, 1.0, chan(FL)),
                        src(35.0, 1.0, chan(FR)),
                        src(0.0, 1.0, chan(FC)),
                        src(-100.0, 1.0, chan(SL)),
                        src(100.0, 1.0, chan(SR)),
                        src(-145.0, 1.0, chan(RL)),
                        src(145.0, 1.0, chan(RR)),
                    ],
                    lfe: chan(LFE),
                }),
                Mode::Surround => {
                    // Stereo spread: left/right wide at the front, a little centre for vocals,
                    // and the left−right "room" sound delayed and softened behind you.
                    let centre = left.iter().zip(&right).map(|(l, r)| (l + r) * 0.5).collect();
                    Some(Scene {
                        sources: vec![
                            src(-50.0, 1.0, left),
                            src(50.0, 1.0, right),
                            src(0.0, 0.6, centre),
                            src(-115.0, 0.7, amb_l),
                            src(115.0, 0.7, amb_r),
                        ],
                        lfe: Vec::new(),
                    })
                }
                Mode::Rotate | Mode::Position => {
                    let half = self.settings.width_deg * 0.5;
                    let g = std::f32::consts::FRAC_1_SQRT_2;
                    Some(Scene {
                        sources: vec![
                            Source { a0: theta0 - half, a1: theta1 - half, gain: g, pcm: left },
                            Source { a0: theta0 + half, a1: theta1 + half, gain: g, pcm: right },
                        ],
                        lfe: Vec::new(),
                    })
                }
            }
        });

        Processed { stereo, scene, layout: self.layout.clone() }
    }
}

/// A short two-tone chime at each test angle in turn; the music is muted meanwhile.
fn test_scene(test: &TestRun, t_play: f64, frames: usize) -> Scene {
    let mut sources: Vec<Source> = Vec::new();
    for i in 0..frames {
        let t = t_play + i as f64 * 1e6 / RATE as f64 - test.start_us;
        if t < 0.0 {
            continue;
        }
        let k = (t / test.step_us) as usize;
        let Some(&angle) = test.angles.get(k) else { continue };
        let local = ((t - k as f64 * test.step_us) / 1e6) as f32;
        let env = (-local / 0.18).exp() * 0.5;
        let x = env * ((2.0 * PI * 880.0 * local).sin() + 0.3 * (2.0 * PI * 1760.0 * local).sin());
        if sources.last().is_none_or(|s| s.a0 != angle) {
            sources.push(Source { a0: angle, a1: angle, gain: 1.0, pcm: vec![0.0; frames] });
        }
        sources.last_mut().unwrap().pcm[i] = x;
    }
    Scene { sources, lfe: Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(angles: &[f32]) -> Layout {
        Layout::new(angles.iter().enumerate().map(|(i, &a)| (Member::Client(i as u64), a)).collect())
    }

    #[test]
    fn source_on_a_device_plays_only_there() {
        let l = ring(&[0.0, 90.0, 180.0, 270.0]);
        assert!((l.gain(Member::Client(1), 90.0) - 1.0).abs() < 1e-6);
        for other in [0, 2, 3] {
            assert!(l.gain(Member::Client(other), 90.0).abs() < 1e-6);
        }
    }

    /// PC at 0°, devices 1..3 at 90°, 180°, 270°; returns each member's peak for one chunk.
    fn peaks(settings: Settings, frame: [f32; CAPTURE_CHANNELS], chunks: usize) -> [f32; 4] {
        let shared = Arc::new(Shared::default());
        *shared.settings.lock().unwrap() = settings;
        let members = vec![(Member::Pc, 0.0), (Member::Client(1), 90.0), (Member::Client(2), 180.0), (Member::Client(3), 270.0)];
        *shared.layout.lock().unwrap() = Arc::new(Layout::new(members));
        let mut builder = SceneBuilder::new(shared);
        let pcm: Vec<f32> = frame.iter().copied().cycle().take(256 * CAPTURE_CHANNELS).collect();
        let mut out = [0.0; 4];
        for k in 0..chunks {
            let p = builder.process(&pcm, k as f64 * 5333.0);
            let scene = p.scene.expect("3D mode makes a scene");
            for (i, m) in [Member::Pc, Member::Client(1), Member::Client(2), Member::Client(3)].into_iter().enumerate() {
                out[i] = mix(&scene, &p.layout, m, 256).iter().fold(0.0f32, |a, x| a.max(x.abs()));
            }
        }
        out
    }

    fn close(actual: [f32; 4], expected: [f32; 4]) {
        for (a, e) in actual.iter().zip(expected) {
            assert!((a - e).abs() < 0.01, "got {actual:?}, expected {expected:?}");
        }
    }

    #[test]
    fn stereo_spread_wraps_the_front_and_keeps_mono_content_out_of_the_back() {
        let s = Settings { mode: Mode::Surround, ..Default::default() };
        let mut f = [0.0; CAPTURE_CHANNELS];
        f[FL] = 0.5;
        f[FR] = 0.5;
        // R at 50° → 90° device sin(50/90·π/2)=0.766; L mirrors it to 270°; PC gets both + centre.
        let side = 0.5 * (50.0f32 / 90.0 * FRAC_PI_2).sin();
        let pc = 2.0 * 0.5 * (50.0f32 / 90.0 * FRAC_PI_2).cos() + 0.6 * 0.5;
        close(peaks(s, f, 20), [pc, side, 0.0, side]);
    }

    #[test]
    fn real_surround_channels_play_from_their_direction() {
        let s = Settings { mode: Mode::Surround, ..Default::default() };
        let mut f = [0.0; CAPTURE_CHANNELS];
        f[RL] = 0.5; // rear-left sits at 215°: between the 180° and 270° devices
        let t = 35.0f32 / 90.0;
        close(peaks(s, f, 2), [0.0, 0.0, 0.5 * (t * FRAC_PI_2).cos(), 0.5 * (t * FRAC_PI_2).sin()]);
    }

    #[test]
    fn position_mode_puts_the_music_on_the_chosen_device() {
        let s = Settings { mode: Mode::Position, target_deg: 90.0, width_deg: 0.0, ..Default::default() };
        let mut f = [0.0; CAPTURE_CHANNELS];
        f[FL] = 0.5;
        f[FR] = 0.5;
        // After gliding there, both halves (each at −3 dB) land on the 90° device.
        close(peaks(s, f, 100), [0.0, 2.0 * 0.5 * std::f32::consts::FRAC_1_SQRT_2, 0.0, 0.0]);
    }

    #[test]
    fn rotation_advances_by_lap_time() {
        let shared = Arc::new(Shared::default());
        *shared.settings.lock().unwrap() = Settings { mode: Mode::Rotate, lap_s: 4.0, ..Default::default() };
        let mut builder = SceneBuilder::new(shared.clone());
        let pcm = vec![0.0; 240 * CAPTURE_CHANNELS]; // 5 ms
        for k in 0..200 {
            builder.process(&pcm, k as f64 * 5000.0);
        }
        // 1 s at 4 s per lap = 90° clockwise.
        assert!((shared.theta() - 90.0).abs() < 0.01, "theta {}", shared.theta());
    }

    #[test]
    fn power_is_constant_between_devices_including_wraparound() {
        let l = ring(&[0.0, 90.0, 180.0, 270.0]);
        for src in [10.0, 45.0, 135.0, 300.0, 359.0, -20.0] {
            let power: f32 = (0..4).map(|i| l.gain(Member::Client(i), src).powi(2)).sum();
            assert!((power - 1.0).abs() < 1e-5, "src {src}: power {power}");
        }
        // Halfway between 270° and 0° is shared equally by those two.
        let g = l.gain(Member::Client(3), 315.0);
        assert!((g - l.gain(Member::Client(0), 315.0)).abs() < 1e-6 && g > 0.7);
    }
}
