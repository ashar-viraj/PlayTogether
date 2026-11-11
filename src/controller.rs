//! Automatic room delay: as small as the slowest device allows.
//!
//! Every receiver (and this PC's own output) reports its headroom: how much earlier than needed
//! its audio arrived, minus a small safety margin. The delay grows right away when anyone runs
//! short or glitches, and shrinks after a calm stretch. A glitch right after we shrank it doubles
//! how long that calm stretch has to be, so a flaky Wi-Fi doesn't cause a glitch every few
//! seconds. Glitches from devices joining don't count against shrinking.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use crate::state::App;

/// Safety margin for the PC's own output on top of what it measures.
const PC_MARGIN_MS: f64 = 1.5;
const MIN_CALM: Duration = Duration::from_secs(5);
const MAX_CALM: Duration = Duration::from_secs(300);

pub async fn run(app: Arc<App>) {
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut last_change = Instant::now();
    let mut calm_since: Option<Instant> = None;
    let mut calm_needed = MIN_CALM;
    let mut last_decrease: Option<Instant> = None;
    let mut seen_late: HashMap<u64, u64> = HashMap::new();
    let mut seen_underruns = 0u64;

    loop {
        tick.tick().await;

        let slack_us = app.ctl.pc_min_slack_us.swap(i64::MAX, Relaxed);
        let pc_headroom = (slack_us != i64::MAX).then(|| slack_us as f64 / 1000.0 - PC_MARGIN_MS);
        let underruns = app.ctl.pc_underruns.load(Relaxed);
        {
            let mut pc = app.pc.lock().unwrap();
            pc.headroom_ms = pc_headroom;
            pc.out_latency_ms = app.ctl.pc_out_latency_us.load(Relaxed) as f64 / 1000.0;
            pc.underruns = underruns;
            pc.level = app.ctl.take_peak();
        }

        let mut headrooms = Vec::new();
        let mut glitched = false;
        if !app.ctl.pc_muted.load(Relaxed) {
            headrooms.extend(pc_headroom);
            glitched |= underruns > seen_underruns;
        }
        seen_underruns = underruns;
        {
            let clients = app.clients.lock().unwrap();
            seen_late.retain(|id, _| clients.contains_key(id));
            for (id, c) in clients.iter() {
                if !c.stats.playing || c.last_seen.elapsed() > Duration::from_secs(3) {
                    continue;
                }
                headrooms.extend(c.stats.headroom_ms);
                let before = seen_late.insert(*id, c.stats.late);
                glitched |= before.is_some_and(|b| c.stats.late > b);
            }
        }

        if !app.delay_auto.load(Relaxed) {
            continue;
        }
        let Some(worst) = headrooms.into_iter().reduce(f64::min) else { continue };
        let delay_ms = app.ctl.delay_us.load(Relaxed) as f64 / 1000.0;
        let since_change = last_change.elapsed();

        let mut target = delay_ms;
        if worst < 0.0 || glitched {
            calm_since = None;
            // Receivers drop their old readings when they see the delay change, so one report
            // interval is enough for a change to show up.
            if since_change > Duration::from_secs(1) {
                target = delay_ms + (-worst).max(0.0) + if glitched { 10.0 } else { 4.0 };
                if glitched && last_decrease.is_some_and(|t| t.elapsed() < Duration::from_secs(20)) {
                    calm_needed = (calm_needed * 2).min(MAX_CALM);
                }
            }
        } else if worst > 8.0 && since_change > Duration::from_secs(3) {
            let since = *calm_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= calm_needed {
                let excess = worst - 4.0;
                // Far too much delay (e.g. a slow device left): one jump down. Otherwise small
                // steps that the ±0.3% speed correction absorbs without an audible jump.
                target = delay_ms - if excess > 60.0 { excess * 0.6 } else { (excess * 0.5).min(12.0) };
                calm_since = None;
                last_decrease = Some(Instant::now());
            }
        } else if worst <= 8.0 {
            calm_since = None;
        }

        if (target - delay_ms).abs() >= 1.0 {
            app.set_delay_ms(target);
            last_change = Instant::now();
        }
    }
}
