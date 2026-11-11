//! Switching the system default output to our virtual sink and back, through WirePlumber's
//! "default" metadata (the same thing `wpctl set-default` does).

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, bail};

use crate::audio::SINK_NAME;

const CONFIGURED_KEY: &str = "default.configured.audio.sink";
const EFFECTIVE_KEY: &str = "default.audio.sink";

/// The sink the user picked (may be absent) and the sink actually in use right now.
pub struct Defaults {
    pub configured: Option<String>,
    pub effective: Option<String>,
}

pub fn read() -> anyhow::Result<Defaults> {
    let out = Command::new("pw-metadata")
        .args(["-n", "default", "0"])
        .output()
        .context("can't run pw-metadata (is pipewire installed and running?)")?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(Defaults { configured: find_name(&text, CONFIGURED_KEY), effective: find_name(&text, EFFECTIVE_KEY) })
}

/// Pulls `name` out of a line like `update: id:0 key:'<key>' value:'{"name":"x"}' type:'Spa:String:JSON'`.
fn find_name(text: &str, key: &str) -> Option<String> {
    let needle = format!("key:'{key}' value:'");
    let line = text.lines().find(|l| l.contains(&needle))?;
    let value = line.split(&needle).nth(1)?;
    let json = &value[..value.rfind("' type:")?];
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v["name"].as_str().map(str::to_owned)
}

pub fn set_configured(name: Option<&str>) -> anyhow::Result<()> {
    let status = match name {
        Some(name) => {
            let value = serde_json::json!({ "name": name }).to_string();
            Command::new("pw-metadata")
                .args(["-n", "default", "0", CONFIGURED_KEY, &value, "Spa:String:JSON"])
                .output()?
                .status
        }
        None => Command::new("pw-metadata").args(["-n", "default", "-d", "0", CONFIGURED_KEY]).output()?.status,
    };
    if !status.success() {
        bail!("pw-metadata failed to update the default sink");
    }
    Ok(())
}

fn state_file() -> PathBuf {
    crate::state::state_dir().join("previous-default-sink")
}

/// Records the user's own default sink so it can be restored even after a crash left ours
/// configured. Returns the sink to restore on exit.
pub fn remember_user_default(configured: Option<String>) -> Option<String> {
    let path = state_file();
    if configured.as_deref() == Some(SINK_NAME) {
        // Left over from a run that didn't exit cleanly.
        let saved = std::fs::read_to_string(&path).ok()?;
        let saved = saved.trim();
        return (!saved.is_empty()).then(|| saved.to_owned());
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, configured.as_deref().unwrap_or(""));
    configured
}

pub fn forget_user_default() {
    let _ = std::fs::remove_file(state_file());
}
