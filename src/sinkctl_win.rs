//! Windows counterpart of the PipeWire virtual sink: the VB-Audio Virtual Cable.
//!
//! While PlayTogether runs, the cable is the Windows default output, so every app plays into it
//! (silently). We capture the cable's other end and play this PC's share on the real speakers on
//! the room schedule. On exit the previous default output is restored. Without the cable
//! installed, nothing is switched and the real output is captured with loopback instead.
//!
//! The default output is changed through `IPolicyConfig`, the undocumented-but-stable COM
//! interface the Windows Sound control panel itself uses.

use std::ffi::c_void;
use std::path::PathBuf;

use anyhow::{Context, anyhow};
use windows::Win32::Devices::Properties::DEVPKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, EDataFlow, ERole, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture,
    eCommunications, eConsole, eMultimedia, eRender,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ,
};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::core::{GUID, HRESULT, IUnknown, Interface, PCWSTR};

use crate::audio::SINK_NAME;

const CABLE_TAG: &str = "VB-Audio Virtual Cable";

pub struct Defaults {
    /// Endpoint ID of the current default output, or `SINK_NAME` if that's already the cable.
    pub configured: Option<String>,
    /// Friendly name of the real output the PC should play on.
    pub effective: Option<String>,
}

struct Endpoint {
    id: String,
    name: String,
}

/// Runs `f` on a fresh MTA thread, so COM state never clashes with whatever thread called us.
fn com<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        f()
    })
    .join()
    .map_err(|_| anyhow!("audio device thread panicked"))?
}

fn enumerator() -> anyhow::Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.context("can't open the audio device list")
}

fn endpoint(dev: &IMMDevice) -> anyhow::Result<Endpoint> {
    unsafe {
        let raw = dev.GetId()?;
        let id = raw.to_string()?;
        CoTaskMemFree(Some(raw.0 as *const c_void));

        let store = dev.OpenPropertyStore(STGM_READ)?;
        let mut pv = store.GetValue(&DEVPKEY_Device_FriendlyName as *const _ as *const _)?;
        let inner = &pv.as_raw().Anonymous.Anonymous;
        let name = if inner.vt == VT_LPWSTR.0 {
            let p = *(&inner.Anonymous as *const _ as *const *const u16);
            PCWSTR(p).to_string().unwrap_or_default()
        } else {
            String::new()
        };
        let _ = PropVariantClear(&mut pv);
        Ok(Endpoint { id, name })
    }
}

fn list(flow: EDataFlow) -> anyhow::Result<Vec<Endpoint>> {
    unsafe {
        let all = enumerator()?.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
        let mut out = Vec::new();
        for i in 0..all.GetCount()? {
            if let Ok(e) = endpoint(&all.Item(i)?) {
                out.push(e);
            }
        }
        Ok(out)
    }
}

/// Finds an active device by friendly name: exact match first, then a partial match that is
/// never the virtual cable (playing into it by accident would loop our output back in).
/// Call on a thread that has COM initialised.
pub fn open_device(render: bool, name: &str) -> anyhow::Result<IMMDevice> {
    let flow = if render { eRender } else { eCapture };
    unsafe {
        let all = enumerator()?.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
        let mut partial = None;
        for i in 0..all.GetCount()? {
            let dev = all.Item(i)?;
            let Ok(e) = endpoint(&dev) else { continue };
            if e.name == name {
                return Ok(dev);
            }
            if partial.is_none() && !e.name.contains("VB-Audio") && e.name.to_lowercase().contains(&name.to_lowercase()) {
                partial = Some(dev);
            }
        }
        partial.ok_or_else(|| anyhow!("no {} device named \"{name}\"", if render { "output" } else { "recording" }))
    }
}

/// True if an active recording device has exactly this name.
pub fn is_recording_device(name: &str) -> bool {
    list(eCapture).is_ok_and(|l| l.iter().any(|e| e.name == name))
}

fn default_render() -> anyhow::Result<Option<Endpoint>> {
    match unsafe { enumerator()?.GetDefaultAudioEndpoint(eRender, eConsole) } {
        Ok(dev) => Ok(Some(endpoint(&dev)?)),
        Err(_) => Ok(None),
    }
}

fn is_cable_render(e: &Endpoint) -> bool {
    e.name.contains(CABLE_TAG) && !e.name.contains("16 Ch")
}

/// The cable's playback side (what apps play into).
fn cable_render() -> Option<Endpoint> {
    com(|| list(eRender)).ok()?.into_iter().find(is_cable_render)
}

/// Name of the cable's recording side, if the cable is installed.
pub fn cable_capture_name() -> Option<String> {
    let caps = com(|| list(eCapture)).ok()?;
    caps.into_iter().find(|e| e.name.contains(CABLE_TAG) && !e.name.contains("16 Ch")).map(|e| e.name)
}

pub fn read() -> anyhow::Result<Defaults> {
    com(|| {
        let Some(def) = default_render()? else {
            return Ok(Defaults { configured: None, effective: None });
        };
        if !is_cable_render(&def) {
            return Ok(Defaults { configured: Some(def.id), effective: Some(def.name) });
        }
        // The cable is already the default: left over from a crash (use the saved real output),
        // or the VB-Cable installer made it the default (use the first real output, and hand
        // that back as the user's default so it's restored on exit).
        let saved = std::fs::read_to_string(state_file()).unwrap_or_default();
        let saved = saved.trim().to_owned();
        let outputs = list(eRender)?;
        if let Some(e) = outputs.iter().find(|e| !saved.is_empty() && e.id == saved) {
            return Ok(Defaults { configured: Some(SINK_NAME.to_owned()), effective: Some(e.name.clone()) });
        }
        match outputs.into_iter().find(|e| !e.name.contains("VB-Audio")) {
            Some(real) => Ok(Defaults { configured: Some(real.id), effective: Some(real.name) }),
            None => Ok(Defaults { configured: None, effective: None }),
        }
    })
}

/// `Some(SINK_NAME)` makes the cable the default output; `Some(id)` restores that endpoint.
pub fn set_configured(name: Option<&str>) -> anyhow::Result<()> {
    let Some(name) = name else { return Ok(()) };
    let id = if name == SINK_NAME {
        match cable_render() {
            Some(c) => c.id,
            None => return Ok(()), // no cable: loopback mode, nothing to switch
        }
    } else {
        name.to_owned()
    };
    com(move || set_default_endpoint(&id))
}

const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
const IID_POLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
/// IUnknown (3) + GetMixFormat … SetPropertyValue (10) → SetDefaultEndpoint is slot 13.
const SET_DEFAULT_ENDPOINT_SLOT: usize = 13;
type SetDefaultEndpoint = unsafe extern "system" fn(*mut c_void, PCWSTR, ERole) -> HRESULT;

fn set_default_endpoint(id: &str) -> anyhow::Result<()> {
    let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let unk: IUnknown = CoCreateInstance(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL)
            .context("can't reach Windows audio policy")?;
        let mut pc: *mut c_void = std::ptr::null_mut();
        unk.query(&IID_POLICY_CONFIG, &mut pc).ok().context("IPolicyConfig unavailable")?;
        let vtbl = *(pc as *const *const usize);
        let set: SetDefaultEndpoint = std::mem::transmute(*vtbl.add(SET_DEFAULT_ENDPOINT_SLOT));
        let mut result = Ok(());
        for role in [eConsole, eMultimedia, eCommunications] {
            if let Err(e) = set(pc, PCWSTR(wide.as_ptr()), role).ok() {
                result = Err(anyhow!("can't change the default output: {e}"));
            }
        }
        // Release the reference `query` added.
        let release: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vtbl.add(2));
        release(pc);
        let _ = unk.as_raw();
        result
    }
}

fn state_file() -> PathBuf {
    crate::state::state_dir().join("previous-default-output")
}

/// Records the user's own default output so it can be restored even after a crash left the
/// cable as default. Returns the endpoint to restore on exit.
pub fn remember_user_default(configured: Option<String>) -> Option<String> {
    let path = state_file();
    if configured.as_deref() == Some(SINK_NAME) {
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
