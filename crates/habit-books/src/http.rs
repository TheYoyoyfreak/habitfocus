//! HTTP via the system `curl`, like the rest of habitfocus: no TLS stack in our binaries.

use std::path::Path;
use std::process::Command;

use anyhow::Context;

const USER_AGENT: &str = concat!("habitfocus/", env!("CARGO_PKG_VERSION"));

fn curl() -> Command {
    let mut cmd = Command::new("curl");
    cmd.args(["-fsSL", "--max-time", "20", "-A", USER_AGENT]);
    cmd
}

fn run(mut cmd: Command) -> anyhow::Result<Vec<u8>> {
    let out = cmd.output().context("running curl (is it installed?)")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("{}", err.trim().trim_start_matches("curl: "));
    }
    Ok(out.stdout)
}

/// GET `url` with URL-encoded query parameters.
pub fn get(url: &str, query: &[(&str, &str)]) -> anyhow::Result<Vec<u8>> {
    let mut cmd = curl();
    cmd.arg("--get");
    for (k, v) in query {
        cmd.arg("--data-urlencode").arg(format!("{k}={v}"));
    }
    cmd.arg(url);
    run(cmd)
}

/// Downloads `url` to `dest`; a partial download never ends up at `dest`.
pub fn download(url: &str, dest: &Path) -> anyhow::Result<()> {
    let tmp = dest.with_extension("part");
    let mut cmd = curl();
    cmd.arg("-o").arg(&tmp).arg(url);
    run(cmd)?;
    std::fs::rename(&tmp, dest)?;
    Ok(())
}
