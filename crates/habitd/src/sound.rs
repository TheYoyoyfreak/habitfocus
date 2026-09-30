//! Playing the short sound of a finished habit. `general.sound` is a sound
//! name from the system's theme ("complete", "bell", …), a path to a file, or
//! empty for silence.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Where sound names are looked up, most specific first.
fn sound_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
    let mut dirs: Vec<PathBuf> = data.into_iter().collect();
    dirs.extend([PathBuf::from("/usr/local/share"), PathBuf::from("/usr/share")]);
    dirs.into_iter().flat_map(|base| [base.join("sounds/freedesktop/stereo"), base.join("sounds")]).collect()
}

/// The file a sound setting names: a path as it is, or `<dir>/<name>.<ext>`.
pub fn resolve(sound: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let sound = sound.trim();
    if sound.is_empty() {
        return None;
    }
    if sound.contains('/') || sound.contains('.') {
        let path = PathBuf::from(shellexpand(sound));
        return path.is_file().then_some(path);
    }
    dirs.iter()
        .flat_map(|dir| ["oga", "ogg", "wav", "mp3", "flac"].map(|ext| dir.join(format!("{sound}.{ext}"))))
        .find(|path| path.is_file())
}

/// `~` in a configured path.
fn shellexpand(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => format!("{}/{rest}", home.to_string_lossy()),
        _ => path.to_string(),
    }
}

/// The player to use, found once: the program and the arguments before the file.
fn player() -> Option<&'static (String, Vec<String>)> {
    static PLAYER: OnceLock<Option<(String, Vec<String>)>> = OnceLock::new();
    PLAYER
        .get_or_init(|| {
            let candidates: [(&str, &[&str]); 5] = [
                ("pw-play", &[]),
                ("paplay", &[]),
                ("ffplay", &["-nodisp", "-autoexit", "-loglevel", "quiet"]),
                ("mpv", &["--really-quiet", "--no-video"]),
                ("aplay", &["-q"]),
            ];
            let found = candidates.into_iter().find(|(program, _)| which(program));
            match found {
                Some((program, args)) => {
                    Some((program.to_string(), args.iter().map(|a| a.to_string()).collect()))
                }
                // Without a player, the theme's own tool can still play a name.
                None => which("canberra-gtk-play").then(|| ("canberra-gtk-play".to_string(), vec!["-i".to_string()])),
            }
        })
        .as_ref()
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// Plays `sound` in the background. Failures are quiet: a missing sound
/// shouldn't interrupt anything.
pub fn play(sound: &str) {
    let Some((program, args)) = player() else { return };
    let argument = match resolve(sound, &sound_dirs()) {
        Some(file) => file.into_os_string(),
        // canberra plays a name; the others need a file.
        None if program == "canberra-gtk-play" => sound.into(),
        None => {
            eprintln!("habitd: no sound file for {sound:?}; set general.sound to a path or \"\"");
            return;
        }
    };
    let mut command = tokio::process::Command::new(program);
    command.args(args).arg(argument);
    command.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    tokio::spawn(async move {
        let _ = command.status().await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_names_and_paths() {
        let dir = std::env::temp_dir().join(format!("hf-sound-{}", std::process::id()));
        let theme = dir.join("sounds/freedesktop/stereo");
        std::fs::create_dir_all(&theme).unwrap();
        let file = theme.join("ding.oga");
        std::fs::write(&file, b"not really audio").unwrap();
        let dirs = vec![theme.clone()];

        assert_eq!(resolve("ding", &dirs), Some(file.clone()));
        assert_eq!(resolve("  ding  ", &dirs), Some(file.clone()));
        assert_eq!(resolve("nope", &dirs), None);
        assert_eq!(resolve("", &dirs), None);
        // A path is taken as it is, and only when it exists.
        assert_eq!(resolve(file.to_str().unwrap(), &[]), Some(file));
        assert_eq!(resolve("/nowhere/ding.oga", &[]), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn expands_home() {
        let home = std::env::var("HOME").unwrap_or_default();
        assert_eq!(shellexpand("~/a.oga"), format!("{home}/a.oga"));
        assert_eq!(shellexpand("/tmp/a.oga"), "/tmp/a.oga");
    }
}
