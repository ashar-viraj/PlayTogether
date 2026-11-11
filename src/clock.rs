/// Host clock in microseconds (CLOCK_MONOTONIC), the same clock PipeWire stamps its
/// graph cycles with. Every timestamp on the wire is in this timebase.
#[cfg(unix)]
pub fn now_us() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointer to a timespec; CLOCK_MONOTONIC always exists on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec * 1_000_000 + ts.tv_nsec / 1_000
}

/// Host clock in microseconds (QueryPerformanceCounter), the same clock WASAPI stamps its
/// buffers and device positions with. Every timestamp on the wire is in this timebase.
#[cfg(windows)]
pub fn now_us() -> i64 {
    use std::sync::OnceLock;
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    static FREQ: OnceLock<i64> = OnceLock::new();
    let freq = *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: valid out-pointer; QPC always exists on supported Windows versions.
        let _ = unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(1)
    });
    let mut c = 0i64;
    // SAFETY: as above.
    let _ = unsafe { QueryPerformanceCounter(&mut c) };
    (c as i128 * 1_000_000 / freq as i128) as i64
}

/// WASAPI reports QPC positions in 100 ns units; this puts them on the `now_us` timebase.
#[cfg(windows)]
pub fn qpc_100ns_to_us(qpc_100ns: u64) -> f64 {
    qpc_100ns as f64 / 10.0
}
