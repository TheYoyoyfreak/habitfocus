//! Fallback enforcement for blocked processes that have no window (yet).

use habit_core::Engine;

/// Sends SIGTERM to every process of this user whose name is blocked.
pub fn enforce(engine: &Engine, now: u64) {
    let Ok(entries) = std::fs::read_dir("/proc") else { return };
    let own_pid = std::process::id();
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == own_pid {
            continue;
        }
        let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        let comm = comm.trim_end();
        if engine.is_process_blocked(comm, now) {
            // Fails harmlessly for other users' processes.
            let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
            eprintln!("habitd: terminated blocked process {comm} ({pid})");
        }
    }
}
