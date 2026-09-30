//! `hf setup`: wires up an installed habitfocus. Writes the example config if
//! there is none, the systemd user unit for the `habitd` next to this `hf`, and
//! the browser native host; `--start` (re)starts the daemon, `--uninstall`
//! undoes the service and host. Used by `install.sh` and after `cargo install`.

use anyhow::{bail, Context};
use std::path::{Path, PathBuf};

/// The unit as shipped; `hf setup` points its `ExecStart` at the real binary.
const UNIT_TEMPLATE: &str = include_str!("../../../contrib/habitd.service");
const TEMPLATE_EXEC: &str = "%h/.cargo/bin/habitd";

/// The unit for `habitd` at `habitd`.
fn render_unit(habitd: &Path) -> String {
    let path = habitd.display().to_string();
    // systemd splits ExecStart on spaces unless the path is quoted.
    let exec = if path.contains(char::is_whitespace) { format!("\"{path}\"") } else { path };
    UNIT_TEMPLATE.replace(TEMPLATE_EXEC, &exec)
}

/// `habitd` next to `hf` (both come from one release or one `cargo install`),
/// else the first one on `PATH`.
fn find_habitd(hf: &Path) -> Option<PathBuf> {
    let beside = hf.parent().map(|dir| dir.join("habitd")).filter(|p| p.is_file());
    beside.or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join("habitd")).find(|p| p.is_file())
    })
}

/// Writes the unit if it differs; whether it changed.
fn write_unit(path: &Path, text: &str) -> anyhow::Result<bool> {
    if std::fs::read_to_string(path).is_ok_and(|current| current == text) {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(true)
}

/// Runs `systemctl --user …`. `HABITFOCUS_NO_SYSTEMCTL` only prints it (tests,
/// containers, trying the installer in a throwaway HOME).
fn systemctl(args: &[&str]) -> bool {
    if std::env::var_os("HABITFOCUS_NO_SYSTEMCTL").is_some() {
        println!("(skipped) systemctl --user {}", args.join(" "));
        return true;
    }
    std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdout(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn run(start: bool, uninstall: bool) -> anyhow::Result<()> {
    let unit = habit_ipc::unit_path();
    if uninstall {
        systemctl(&["disable", "--now", "habitd"]);
        if std::fs::remove_file(&unit).is_ok() {
            println!("removed {}", unit.display());
        }
        systemctl(&["daemon-reload"]);
        if let Err(e) = crate::native_host::install(true) {
            eprintln!("hf: browser host: {e:#}");
        }
        println!("habitd is stopped and removed. Your config and data stay in {} and {}.",
            habit_ipc::config_path().display(),
            habit_ipc::state_path().parent().map_or_else(String::new, |d| d.display().to_string()));
        return Ok(());
    }

    let fresh_config = crate::init_config()?;
    let config = habit_ipc::config_path();
    if fresh_config {
        println!("wrote {} (the example: edit it before starting habitd)", config.display());
    }

    let hf = std::env::current_exe()?.canonicalize()?;
    let Some(habitd) = find_habitd(&hf) else {
        bail!("habitd isn't next to {} or on PATH; install both binaries", hf.display());
    };
    if write_unit(&unit, &render_unit(&habitd))? {
        println!("wrote {} (runs {})", unit.display(), habitd.display());
        if !systemctl(&["daemon-reload"]) {
            eprintln!("hf: `systemctl --user daemon-reload` failed; is this a systemd session?");
        }
    }

    // A missing browser is no reason to fail the rest.
    if let Err(e) = crate::native_host::install(false) {
        eprintln!("hf: browser host not registered: {e:#}");
    }

    if start {
        let running = systemctl(&["is-active", "--quiet", "habitd"]);
        let (verb, args): (&str, &[&str]) =
            if running { ("restarted", &["restart", "habitd"]) } else { ("started", &["enable", "--now", "habitd"]) };
        if !systemctl(args) {
            bail!("couldn't start habitd; see `systemctl --user status habitd`");
        }
        println!("habitd {verb}");
        return Ok(());
    }
    let url = format!("https://github.com/{}/releases/latest", habit_ipc::repo());
    println!();
    println!("Next:");
    println!("  1. Edit {} (`hf tui`, then `e` on a block or habit, works too)", config.display());
    println!("  2. systemctl --user enable --now habitd      (or `hf setup --start`)");
    println!("  3. Browser extension for blocking sites: {url}");
    println!("     Firefox/Zen: habitfocus-firefox.xpi · Chrome/Brave: habitfocus-chromium.zip, load unpacked");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_runs_the_installed_habitd() {
        let unit = render_unit(Path::new("/home/me/.local/bin/habitd"));
        assert!(unit.contains("ExecStart=/home/me/.local/bin/habitd\n"), "{unit}");
        assert!(unit.contains("WantedBy=graphical-session.target"));
        assert!(UNIT_TEMPLATE.contains(TEMPLATE_EXEC), "contrib/habitd.service changed its ExecStart");
        let spaced = render_unit(Path::new("/opt/my apps/habitd"));
        assert!(spaced.contains("ExecStart=\"/opt/my apps/habitd\"\n"), "{spaced}");
    }

    #[test]
    fn writes_the_unit_only_when_it_changes() {
        let dir = std::env::temp_dir().join(format!("hf-setup-{}", std::process::id()));
        let path = dir.join("systemd/user/habitd.service");
        assert!(write_unit(&path, "a").unwrap());
        assert!(!write_unit(&path, "a").unwrap());
        assert!(write_unit(&path, "b").unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn finds_habitd_next_to_hf() {
        let dir = std::env::temp_dir().join(format!("hf-setup-bin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("habitd"), "").unwrap();
        assert_eq!(find_habitd(&dir.join("hf")), Some(dir.join("habitd")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
