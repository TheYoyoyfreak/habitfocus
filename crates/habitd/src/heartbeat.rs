//! Liveness records that let a restarted habitd tell tampering (`systemctl
//! stop`, `kill`) apart from reboots, suspend and logging out.

use habit_core::lock::Heartbeat;

/// Changes on every boot.
pub fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// CLOCK_MONOTONIC in ms. Unlike CLOCK_BOOTTIME it stops during suspend, so
/// sleeping the machine never counts as downtime.
pub fn monotonic_ms() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime only writes to the provided timespec.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0;
    }
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

pub fn now(wall_ms: u64, clean_shutdown: bool) -> Heartbeat {
    Heartbeat { wall_ms, boot_id: boot_id(), monotonic_ms: monotonic_ms(), clean_shutdown }
}

/// Whether habitd is being stopped because the user session or the system is
/// shutting down, rather than by a targeted `systemctl --user stop habitd`.
pub fn session_is_ending() -> bool {
    let stopping = |args: &[&str]| {
        std::process::Command::new("systemctl")
            .args(args)
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim() == "stopping")
    };
    stopping(&["--user", "is-system-running"]) || stopping(&["is-system-running"])
}

/// Grace period for downtime, overridable for tests.
pub fn tamper_grace_ms() -> u64 {
    std::env::var("HABITFOCUS_TAMPER_GRACE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(habit_core::lock::TAMPER_GRACE_MS)
}
