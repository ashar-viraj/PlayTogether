use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

use crate::audio::Chunk;
use crate::clock;
use crate::net::NetInfo;
use crate::room::{Kind, PC_KEY, Positions, Seat};
use crate::spatial::{self, Member, Mode, TestRun};

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// Bytes per interleaved s16 stereo frame.
pub const FRAME_BYTES: usize = 2 * CHANNELS;
/// Frames per PipeWire cycle we ask for: ~5.3 ms at 48 kHz.
pub const QUANTUM: u32 = 256;

/// Settings and stats shared with the realtime PipeWire callbacks, so lock-free.
pub struct AudioCtl {
    /// Room delay D: every device plays a sample captured at `t` at `t + D`.
    pub delay_us: AtomicI64,
    pub pc_muted: AtomicBool,
    /// Positive = this PC sounds late, so play it earlier.
    pub pc_offset_us: AtomicI64,
    pc_volume: AtomicU32,
    /// Smallest amount of buffered audio the PC output had left since the last read.
    pub pc_min_slack_us: AtomicI64,
    pub pc_out_latency_us: AtomicI64,
    pub pc_underruns: AtomicU64,
    /// Peak capture level since the last read, as f32 bits.
    peak: AtomicU32,
    pub capturing: AtomicBool,
}

impl AudioCtl {
    pub fn new(delay_ms: f64, pc_muted: bool) -> Self {
        Self {
            delay_us: AtomicI64::new((delay_ms * 1000.0) as i64),
            pc_muted: AtomicBool::new(pc_muted),
            pc_offset_us: AtomicI64::new(0),
            pc_volume: AtomicU32::new(1.0f32.to_bits()),
            pc_min_slack_us: AtomicI64::new(i64::MAX),
            pc_out_latency_us: AtomicI64::new(0),
            pc_underruns: AtomicU64::new(0),
            peak: AtomicU32::new(0),
            capturing: AtomicBool::new(false),
        }
    }

    pub fn pc_volume(&self) -> f32 {
        f32::from_bits(self.pc_volume.load(Relaxed))
    }

    pub fn set_pc_volume(&self, v: f32) {
        self.pc_volume.store(v.clamp(0.0, 1.0).to_bits(), Relaxed);
    }

    pub fn report_peak(&self, p: f32) {
        // Non-negative floats order the same as their bit patterns.
        self.peak.fetch_max(p.to_bits(), Relaxed);
    }

    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Relaxed))
    }
}

/// Where PlayTogether keeps its small bits of state (previous default sink, room layout).
pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir)
        .join("Playtogether")
}

/// What a receiver page reports about itself roughly once a second.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientStats {
    pub name: String,
    pub playing: bool,
    pub rtt_ms: Option<f64>,
    /// Worst-case spare time before audio had to be scheduled, minus the page's safety margin.
    /// Negative means the room delay is too small for this device.
    pub headroom_ms: Option<f64>,
    pub late: u64,
    pub out_latency_ms: Option<f64>,
    pub volume: f64,
    pub offset_ms: f64,
    pub muted: bool,
}

pub struct Client {
    pub ip: IpAddr,
    pub is_host: bool,
    /// Stable id the page keeps in localStorage, so a device keeps its seat across reconnects.
    pub device_id: String,
    pub kind: Kind,
    /// Tapped Join and is receiving audio.
    pub joined: bool,
    pub stats: ClientStats,
    pub last_seen: Instant,
    pub tx: mpsc::UnboundedSender<String>,
}

impl Client {
    fn seat_key(&self, id: u64) -> String {
        if self.device_id.is_empty() { format!("anon-{id}") } else { self.device_id.clone() }
    }
}

#[derive(Default, Serialize)]
pub struct PcView {
    pub headroom_ms: Option<f64>,
    pub out_latency_ms: f64,
    pub underruns: u64,
    pub level: f32,
}

pub struct App {
    pub ctl: Arc<AudioCtl>,
    pub spatial: Arc<spatial::Shared>,
    pub audio_tx: broadcast::Sender<Arc<Chunk>>,
    pub clients: Mutex<HashMap<u64, Client>>,
    next_id: AtomicU64,
    positions: Mutex<Positions>,
    pub delay_auto: AtomicBool,
    pub pc: Mutex<PcView>,
    pub pc_output: String,
    /// Kept current by `main`, since Wi-Fi addresses change while we run.
    pub net: Mutex<NetInfo>,
    /// Testing only: let browsers on this PC receive audio too.
    pub allow_local_receivers: bool,
}

/// Dashboard → host commands.
#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Command {
    Delay {
        auto: bool,
        ms: Option<f64>,
    },
    Pc {
        muted: Option<bool>,
        volume: Option<f64>,
        offset_ms: Option<f64>,
    },
    Client {
        id: u64,
        muted: Option<bool>,
        volume: Option<f64>,
        offset_ms: Option<f64>,
    },
    Spatial {
        mode: Option<Mode>,
        lap_s: Option<f32>,
        clockwise: Option<bool>,
        width_deg: Option<f32>,
        target_deg: Option<f32>,
    },
    /// Drag a device (`"pc"` or a client id) to a new seat.
    Place {
        member: Value,
        angle: f32,
    },
    Arrange,
    /// Chime on every device in clockwise order, or just on `member`.
    Test {
        member: Option<Value>,
    },
}

pub const MIN_DELAY_MS: f64 = 15.0;
pub const MAX_DELAY_MS: f64 = 1500.0;
const TEST_STEP_US: f64 = 900_000.0;

fn member_json(m: Member) -> Value {
    match m {
        Member::Pc => json!(PC_KEY),
        Member::Client(id) => json!(id),
    }
}

fn parse_member(v: &Value) -> Option<Member> {
    match v {
        Value::String(s) if s == PC_KEY => Some(Member::Pc),
        v => v.as_u64().map(Member::Client),
    }
}

impl App {
    pub fn new(
        ctl: Arc<AudioCtl>,
        spatial: Arc<spatial::Shared>,
        audio_tx: broadcast::Sender<Arc<Chunk>>,
        delay_auto: bool,
        pc_output: String,
        net: NetInfo,
    ) -> Self {
        Self {
            ctl,
            spatial,
            audio_tx,
            clients: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            positions: Mutex::new(Positions::load()),
            delay_auto: AtomicBool::new(delay_auto),
            pc: Mutex::new(PcView::default()),
            pc_output,
            net: Mutex::new(net),
            allow_local_receivers: false,
        }
    }

    /// True for connections coming from this PC itself (loopback or one of our own addresses).
    pub fn is_local(&self, ip: IpAddr) -> bool {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
            v4 => v4,
        };
        ip.is_loopback() || self.net.lock().unwrap().local_ips.contains(&ip)
    }

    pub fn add_client(&self, ip: IpAddr, is_host: bool, tx: mpsc::UnboundedSender<String>) -> u64 {
        let id = self.next_id.fetch_add(1, Relaxed);
        let client = Client {
            ip,
            is_host,
            device_id: String::new(),
            kind: Kind::default(),
            joined: false,
            stats: ClientStats { volume: 1.0, ..Default::default() },
            last_seen: Instant::now(),
            tx,
        };
        self.clients.lock().unwrap().insert(id, client);
        id
    }

    pub fn identify_client(&self, id: u64, device_id: String, kind: Kind) {
        if let Some(c) = self.clients.lock().unwrap().get_mut(&id) {
            c.device_id = device_id.chars().take(64).collect();
            c.kind = kind;
        }
        self.relayout();
    }

    pub fn set_joined(&self, id: u64, joined: bool) {
        if let Some(c) = self.clients.lock().unwrap().get_mut(&id) {
            c.joined = joined;
        }
        self.relayout();
    }

    pub fn update_client(&self, id: u64, stats: ClientStats) {
        let renamed = {
            let mut clients = self.clients.lock().unwrap();
            let Some(c) = clients.get_mut(&id) else { return };
            let renamed = c.stats.name != stats.name;
            c.stats = stats;
            c.last_seen = Instant::now();
            renamed
        };
        if renamed {
            self.relayout();
        }
    }

    pub fn remove_client(&self, id: u64) {
        self.clients.lock().unwrap().remove(&id);
        self.relayout();
    }

    pub fn set_delay_ms(&self, ms: f64) {
        let ms = ms.clamp(MIN_DELAY_MS, MAX_DELAY_MS);
        self.ctl.delay_us.store((ms * 1000.0) as i64, Relaxed);
    }

    /// Re-seats everyone who's playing and tells each page where it sits.
    /// Lock order everywhere: clients → positions → spatial layout/settings.
    pub fn relayout(&self) {
        let clients = self.clients.lock().unwrap();
        let receivers = clients
            .iter()
            .filter(|(_, c)| c.joined && !c.is_host)
            .map(|(id, c)| Seat { member: Member::Client(*id), key: c.seat_key(*id), kind: c.kind })
            .collect();
        let pc_playing = !self.ctl.pc_muted.load(Relaxed);
        let layout = self.positions.lock().unwrap().seat(pc_playing, receivers);
        *self.spatial.layout.lock().unwrap() = Arc::new(layout);
        self.notify_layout(&clients);
    }

    fn member_name(m: Member, clients: &HashMap<u64, Client>) -> String {
        match m {
            Member::Pc => "This PC".to_owned(),
            Member::Client(id) => clients
                .get(&id)
                .map(|c| c.stats.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("Device {id}")),
        }
    }

    fn notify_layout(&self, clients: &HashMap<u64, Client>) {
        let layout = self.spatial.layout();
        let mode = self.spatial.settings.lock().unwrap().mode;
        let devices: Vec<Value> = layout
            .members
            .iter()
            .map(|&(m, angle)| json!({ "id": member_json(m), "name": Self::member_name(m, clients), "angle": angle }))
            .collect();
        for (id, c) in clients {
            let msg = json!({ "type": "layout", "mode": mode, "you": id, "devices": devices });
            let _ = c.tx.send(msg.to_string());
        }
    }

    pub fn apply_command(self: &Arc<Self>, text: &str) {
        let Ok(cmd) = serde_json::from_str::<Command>(text) else {
            return;
        };
        match cmd {
            Command::Delay { auto, ms } => {
                self.delay_auto.store(auto, Relaxed);
                if let Some(ms) = ms {
                    self.set_delay_ms(ms);
                }
            }
            Command::Pc { muted, volume, offset_ms } => {
                if let Some(v) = volume {
                    self.ctl.set_pc_volume(v as f32);
                }
                if let Some(o) = offset_ms {
                    self.ctl.pc_offset_us.store((o * 1000.0) as i64, Relaxed);
                }
                if let Some(m) = muted {
                    // A muted PC leaves the circle; its neighbours cover its share.
                    self.ctl.pc_muted.store(m, Relaxed);
                    self.relayout();
                }
            }
            Command::Client { id, muted, volume, offset_ms } => {
                let mut clients = self.clients.lock().unwrap();
                let Some(c) = clients.get_mut(&id) else { return };
                // Update our copy right away so the dashboard doesn't flicker back
                // before the page's next stats report arrives.
                if let Some(m) = muted {
                    c.stats.muted = m;
                }
                if let Some(v) = volume {
                    c.stats.volume = v;
                }
                if let Some(o) = offset_ms {
                    c.stats.offset_ms = o;
                }
                let msg = json!({ "type": "set", "muted": muted, "volume": volume, "offset_ms": offset_ms });
                let _ = c.tx.send(msg.to_string());
            }
            Command::Spatial { mode, lap_s, clockwise, width_deg, target_deg } => {
                let mode_changed = {
                    let mut s = self.spatial.settings.lock().unwrap();
                    let before = s.mode;
                    if let Some(m) = mode {
                        s.mode = m;
                    }
                    if let Some(l) = lap_s {
                        s.lap_s = l.clamp(1.0, 60.0);
                    }
                    if let Some(c) = clockwise {
                        s.clockwise = c;
                    }
                    if let Some(w) = width_deg {
                        s.width_deg = w.clamp(0.0, 180.0);
                    }
                    if let Some(t) = target_deg {
                        s.target_deg = spatial::norm(t);
                    }
                    s.mode != before
                };
                if mode_changed {
                    self.notify_layout(&self.clients.lock().unwrap());
                }
            }
            Command::Place { member, angle } => {
                let Some(member) = parse_member(&member) else { return };
                let key = match member {
                    Member::Pc => PC_KEY.to_owned(),
                    Member::Client(id) => match self.clients.lock().unwrap().get(&id) {
                        Some(c) => c.seat_key(id),
                        None => return,
                    },
                };
                self.positions.lock().unwrap().place(&key, angle);
                self.relayout();
            }
            Command::Arrange => {
                self.positions.lock().unwrap().auto_arrange();
                self.relayout();
            }
            Command::Test { member } => self.speaker_test(member.as_ref().and_then(parse_member)),
        }
    }

    /// Plays a chime on each seat clockwise from 12 o'clock (or on one device), and flashes
    /// each receiver's screen on its turn.
    fn speaker_test(self: &Arc<Self>, only: Option<Member>) {
        let layout = self.spatial.layout();
        let seats: Vec<(Member, f32)> =
            layout.members.iter().copied().filter(|(m, _)| only.is_none_or(|o| o == *m)).collect();
        if seats.is_empty() {
            return;
        }
        // Start a little after the audio captured right now will be heard.
        let start_us = (clock::now_us() + self.ctl.delay_us.load(Relaxed)) as f64 + 250_000.0;
        let run = Arc::new(TestRun {
            start_us,
            step_us: TEST_STEP_US,
            angles: seats.iter().map(|&(_, a)| a).collect(),
        });
        {
            let clients = self.clients.lock().unwrap();
            for (k, (m, _)) in seats.iter().enumerate() {
                if let Member::Client(id) = m
                    && let Some(c) = clients.get(id)
                {
                    let at = start_us + k as f64 * TEST_STEP_US;
                    let _ = c.tx.send(json!({ "type": "flash", "at_us": at, "dur_ms": 800 }).to_string());
                }
            }
        }
        let wait = Duration::from_micros((run.end_us() - clock::now_us() as f64).max(0.0) as u64 + 500_000);
        self.spatial.settings.lock().unwrap().test = Some(run.clone());
        let app = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let mut s = app.spatial.settings.lock().unwrap();
            if s.test.as_ref().is_some_and(|t| Arc::ptr_eq(t, &run)) {
                s.test = None;
            }
        });
    }

    /// Everything the dashboard shows, pushed a few times per second.
    pub fn snapshot(&self) -> Value {
        let layout = self.spatial.layout();
        let settings = self.spatial.settings.lock().unwrap().clone();
        let custom = self.positions.lock().unwrap().custom;
        let clients = self.clients.lock().unwrap();
        let mut list: Vec<_> = clients
            .iter()
            .map(|(id, c)| {
                json!({
                    "id": id,
                    "ip": c.ip.to_string(),
                    "is_host": c.is_host,
                    "kind": c.kind,
                    "angle": layout.angle_of(Member::Client(*id)),
                    "stale": c.last_seen.elapsed().as_secs_f64() > 3.0,
                    "stats": c.stats,
                })
            })
            .collect();
        list.sort_by_key(|v| v["id"].as_u64());
        let seats: Vec<Value> = layout
            .members
            .iter()
            .map(|&(m, angle)| json!({ "id": member_json(m), "name": Self::member_name(m, &clients), "angle": angle }))
            .collect();
        drop(clients);

        let pc = self.pc.lock().unwrap();
        json!({
            "type": "state",
            "delay": {
                "auto": self.delay_auto.load(Relaxed),
                "ms": self.ctl.delay_us.load(Relaxed) as f64 / 1000.0,
            },
            "pc": {
                "output": self.pc_output,
                "muted": self.ctl.pc_muted.load(Relaxed),
                "volume": self.ctl.pc_volume(),
                "offset_ms": self.ctl.pc_offset_us.load(Relaxed) as f64 / 1000.0,
                "headroom_ms": pc.headroom_ms,
                "out_latency_ms": pc.out_latency_ms,
                "underruns": pc.underruns,
            },
            "spatial": {
                "settings": settings,
                "theta": self.spatial.theta(),
                "multichannel": self.spatial.multichannel.load(Relaxed),
                "testing": settings.test.is_some(),
                "custom": custom,
            },
            "seats": seats,
            "level": pc.level,
            "capturing": self.ctl.capturing.load(Relaxed),
            "join_urls": self.net.lock().unwrap().join_urls,
            "clients": list,
        })
    }
}
