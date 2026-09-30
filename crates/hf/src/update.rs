//! `hf update`: installs the latest release over this one by running that
//! release's `install.sh` (checksums, binary swap, daemon restart) with this
//! install's prefix. Installs managed by a package manager or cargo are left
//! to them.

use anyhow::bail;
use habit_ipc::update::{Release, CURRENT};
use std::path::Path;

/// Exit code of `hf update --check` when a newer version is out.
pub const UPDATE_AVAILABLE: i32 = 10;

/// Why `hf update` won't replace the binaries in `dir`, with what to run instead.
fn managed_elsewhere(dir: &Path, home: &Path) -> Option<String> {
    if dir.starts_with("/usr") || dir.starts_with("/bin") || dir.starts_with("/opt") {
        return Some(format!("hf in {} belongs to your package manager; update it there", dir.display()));
    }
    if dir.starts_with(home.join(".cargo")) {
        return Some(format!(
            "hf was installed with cargo; update with\n  cargo install --locked --force --git https://github.com/{} habitd hf\n  systemctl --user restart habitd",
            habit_ipc::repo()
        ));
    }
    None
}

/// That release's installer.
fn installer_url(release: &Release) -> String {
    format!("https://github.com/{}/releases/download/{}/install.sh", habit_ipc::repo(), release.tag)
}

/// The installer's arguments for installing `release` into `dir`.
fn installer_args(release: &Release, dir: &Path) -> Vec<String> {
    vec!["--version".into(), release.tag.clone(), "--prefix".into(), dir.display().to_string()]
}

pub fn run(check_only: bool) -> anyhow::Result<()> {
    let release = match habit_ipc::update::latest() {
        Ok(release) => release,
        Err(e) => bail!("{e}"),
    };
    if !habit_ipc::update::is_newer(&release.version, CURRENT) {
        println!("habitfocus {CURRENT} is up to date");
        return Ok(());
    }
    println!("habitfocus {} is available (you have {CURRENT}): {}", release.version, release.url);
    if check_only {
        std::process::exit(UPDATE_AVAILABLE);
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = exe.parent().unwrap_or(Path::new("/"));
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
    if let Some(reason) = managed_elsewhere(dir, &home) {
        bail!("{reason}");
    }
    println!("Updating {CURRENT} → {} in {}", release.version, dir.display());
    // Downloaded first: `curl | sh` would run an empty script when the
    // download fails, and report success.
    let url = installer_url(&release);
    let args = installer_args(&release, dir);
    let by_hand = format!("curl -fsSL {url} | sh -s -- {}", args.join(" "));
    let script = std::env::temp_dir().join(format!("habitfocus-install-{}.sh", std::process::id()));
    let downloaded = std::process::Command::new("curl").arg("-fsSL").arg("-o").arg(&script).arg(&url).status()?;
    if !downloaded.success() {
        bail!("couldn't download {url}");
    }
    let status = std::process::Command::new("sh").arg(&script).args(&args).status();
    let _ = std::fs::remove_file(&script);
    if !status?.success() {
        bail!("the installer failed; you can run it yourself:\n  {by_hand}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_package_manager_and_cargo_installs_alone() {
        let home = Path::new("/home/me");
        assert!(managed_elsewhere(Path::new("/usr/bin"), home).unwrap().contains("package manager"));
        assert!(managed_elsewhere(Path::new("/home/me/.cargo/bin"), home).unwrap().contains("cargo install"));
        assert_eq!(managed_elsewhere(Path::new("/home/me/.local/bin"), home), None);
    }

    #[test]
    fn runs_that_releases_installer_for_this_prefix() {
        let release = Release { tag: "v0.2.0".into(), version: "0.2.0".into(), url: String::new() };
        assert_eq!(
            installer_url(&release),
            "https://github.com/TheYoyoyfreak/habitfocus/releases/download/v0.2.0/install.sh"
        );
        assert_eq!(
            installer_args(&release, Path::new("/home/me/my apps")),
            ["--version", "v0.2.0", "--prefix", "/home/me/my apps"]
        );
    }
}
