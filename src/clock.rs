/// Host clock in microseconds (CLOCK_MONOTONIC), the same clock PipeWire stamps its
/// graph cycles with. Every timestamp on the wire is in this timebase.
pub fn now_us() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointer to a timespec; CLOCK_MONOTONIC always exists on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec * 1_000_000 + ts.tv_nsec / 1_000
}
