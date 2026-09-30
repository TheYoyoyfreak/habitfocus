//! What runs in the focused terminal window, for screen time per program and
//! `program` / `tmux_session` allow rules: the foreground process of the
//! terminal's shell, or of the active tmux pane (and its session) when that
//! process is a tmux client.

use crate::Event;
use habit_core::{Engine, Input};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;

/// Foreground processes that mean "only the shell": the terminal itself.
const SHELLS: &[&str] = &["bash", "zsh", "fish", "sh", "dash", "ksh", "mksh", "tcsh", "csh", "nu", "xonsh", "elvish"];
/// Programs run by an interpreter are named after their script.
const INTERPRETERS: &[&str] = &["node", "nodejs", "bun", "deno", "python", "ruby", "perl"];
/// Script names too generic to name a program; the directory names it instead.
const GENERIC_SCRIPTS: &[&str] = &["cli", "index", "main", "run", "entry", "bin", "dist", "lib", "src", "build", "__main__"];
/// tmux inside tmux (or a pane showing another client) is followed this deep.
const MAX_DEPTH: usize = 3;

/// Looks up what's in front in the focused terminal off the event loop and
/// reports it as `Input::TerminalProgram`.
#[derive(Default)]
pub struct Watch {
    busy: Arc<AtomicBool>,
    /// The terminal window checked last.
    last: Option<u64>,
}

impl Watch {
    /// Checks the focused terminal when `due`, or right away when focus moved
    /// to another terminal window.
    pub fn poll(&mut self, engine: &Engine, due: bool, tx: &Sender<Event>) {
        let Some(window) = engine.focused_terminal() else {
            self.last = None;
            return;
        };
        let Some(pid) = window.pid else { return };
        if (!due && self.last == Some(window.id)) || self.busy.swap(true, Ordering::AcqRel) {
            return;
        }
        self.last = Some(window.id);
        let (id, title, busy, tx) = (window.id, window.title.clone(), self.busy.clone(), tx.clone());
        tokio::task::spawn_blocking(move || {
            let Front { program, tmux_session } = front_of(pid, &title);
            busy.store(false, Ordering::Release);
            let _ = tx.blocking_send(Event::Input(Input::TerminalProgram { window: id, program, tmux_session }));
        });
    }
}

/// What's in front in a terminal window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Front {
    /// The program, e.g. "nvim"; `None` when it's only the shell or can't be told.
    pub program: Option<String>,
    /// The tmux session shown, when a tmux client is in front.
    pub tmux_session: Option<String>,
}

/// The pane a tmux client shows and its session, from the client's argv and pid.
type TmuxLookup<'a> = &'a dyn Fn(&[String], u32) -> Option<(u32, String)>;

/// What's in front in the terminal process `pid`. `title` is the window
/// title, which picks the tab when the terminal has several shells.
pub fn front_of(pid: u32, title: &str) -> Front {
    Procs::read(Path::new("/proc")).front_in(pid, title, &tmux_pane)
}

/// One line of `/proc/<pid>/stat`.
#[derive(Debug, Clone, PartialEq)]
struct Proc {
    ppid: u32,
    pgrp: u32,
    session: u32,
    tty: u32,
    /// Foreground process group of the controlling terminal (-1: none).
    tpgid: i32,
    comm: String,
}

fn parse_stat(text: &str) -> Option<Proc> {
    // The command name is in parentheses and may contain spaces and ')'.
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text.get(open + 1..close)?.to_string();
    let fields: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    // state ppid pgrp session tty_nr tpgid …
    Some(Proc {
        ppid: fields.get(1)?.parse().ok()?,
        pgrp: fields.get(2)?.parse().ok()?,
        session: fields.get(3)?.parse().ok()?,
        tty: fields.get(4)?.parse().ok()?,
        tpgid: fields.get(5)?.parse().ok()?,
        comm,
    })
}

struct Procs {
    root: PathBuf,
    procs: BTreeMap<u32, Proc>,
}

impl Procs {
    fn read(root: &Path) -> Procs {
        let procs = std::fs::read_dir(root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
                let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
                Some((pid, parse_stat(&stat)?))
            })
            .collect();
        Procs { root: root.to_path_buf(), procs }
    }

    fn argv(&self, pid: u32) -> Vec<String> {
        std::fs::read(self.root.join(pid.to_string()).join("cmdline"))
            .map(|raw| {
                raw.split(|&b| b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect()
            })
            .unwrap_or_default()
    }

    /// Shells (session leaders with a terminal) started by `pid`, directly
    /// or through a wrapper like `login`.
    fn shells_of(&self, pid: u32) -> Vec<u32> {
        let mut shells = Vec::new();
        let mut parents = vec![pid];
        for _ in 0..3 {
            let children: Vec<u32> =
                self.procs.iter().filter(|(_, p)| parents.contains(&p.ppid)).map(|(&child, _)| child).collect();
            parents.clear();
            for child in children {
                let p = &self.procs[&child];
                if p.tty != 0 && p.session == child {
                    shells.push(child);
                } else {
                    parents.push(child);
                }
            }
        }
        shells
    }

    /// Leader of the foreground process group on the terminal of `shell`.
    fn foreground(&self, shell: u32) -> Option<u32> {
        let group = u32::try_from(self.procs.get(&shell)?.tpgid).ok().filter(|&g| g > 0)?;
        if self.procs.contains_key(&group) {
            return Some(group);
        }
        // The leader exited: any process still in the group.
        self.procs.iter().find(|(_, p)| p.pgrp == group).map(|(&pid, _)| pid)
    }

    /// What the process `pid` in front counts as; following a tmux client to
    /// its pane (the innermost session wins for tmux in tmux).
    fn front(&self, pid: u32, tmux: TmuxLookup, depth: usize) -> Front {
        let Some(comm) = self.procs.get(&pid).map(|p| &p.comm) else { return Front::default() };
        let argv = self.argv(pid);
        let name = argv.first().map(|a| basename(a).trim_start_matches('-').to_string()).filter(|n| !n.is_empty());
        let name = name.unwrap_or_else(|| comm.clone());
        if name == "tmux" || comm.starts_with("tmux") {
            let Some((pane, session)) = tmux(&argv, pid) else { return Front::default() };
            let inner = match self.foreground(pane) {
                Some(fg) if depth < MAX_DEPTH => self.front(fg, tmux, depth + 1),
                _ => Front::default(),
            };
            return Front { program: inner.program, tmux_session: inner.tmux_session.or(Some(session)) };
        }
        let program = if SHELLS.contains(&name.as_str()) {
            None
        } else if is_interpreter(&name) {
            Some(script_name(&argv).unwrap_or(name))
        } else {
            Some(name)
        };
        Front { program, tmux_session: None }
    }

    fn front_in(&self, terminal: u32, title: &str, tmux: TmuxLookup) -> Front {
        let shells = self.shells_of(terminal);
        let mut candidates: Vec<(u32, Front)> = shells
            .iter()
            .filter_map(|&shell| self.foreground(shell).map(|fg| (shell, self.front(fg, tmux, 0))))
            .collect();
        if candidates.len() > 1 {
            // Several tabs or splits in one terminal process: the one whose
            // program or tmux session the title names, else the one typed into last.
            let title = title.to_lowercase();
            let named = |front: &Front| {
                [&front.program, &front.tmux_session]
                    .into_iter()
                    .flatten()
                    .any(|name| title.contains(&name.to_lowercase()))
            };
            if let Some(i) = candidates.iter().position(|(_, front)| named(front)) {
                return candidates.swap_remove(i).1;
            }
            candidates.sort_by_key(|(shell, _)| std::cmp::Reverse(self.tty_input_time(*shell)));
        }
        candidates.into_iter().next().map(|(_, front)| front).unwrap_or_default()
    }

    /// When the terminal of `shell` was last read from (the kernel keeps
    /// this to within 8 s).
    fn tty_input_time(&self, shell: u32) -> Option<std::time::SystemTime> {
        let tty = self.procs.get(&shell)?.tty;
        let (major, minor) = ((tty >> 8) & 0xfff, (tty & 0xff) | ((tty >> 12) & 0xfff00));
        // Unix98 ptys: majors 136-143.
        let index = (136..=143).contains(&major).then(|| (major - 136) * 256 + minor)?;
        std::fs::metadata(format!("/dev/pts/{index}")).and_then(|m| m.accessed()).ok()
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn is_interpreter(name: &str) -> bool {
    INTERPRETERS.iter().any(|i| name == *i || (name.starts_with(i) && name[i.len()..].chars().all(|c| c.is_ascii_digit() || c == '.')))
}

/// The program an interpreter runs: `node …/codex.js` is "codex",
/// `python -m aider` is "aider", `node …/claude-code/cli.js` is "claude-code".
fn script_name(argv: &[String]) -> Option<String> {
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "-m" {
            return args.next().map(|m| m.split('.').next().unwrap_or(m).to_string());
        }
        if arg.starts_with('-') || arg == "run" {
            continue;
        }
        let mut parts = arg.rsplit('/');
        let file = parts.next()?;
        let stem = file.split_once('.').map_or(file, |(stem, _)| stem);
        if !GENERIC_SCRIPTS.contains(&stem) {
            return Some(stem.to_string());
        }
        return parts.find(|dir| !dir.is_empty() && !GENERIC_SCRIPTS.contains(dir)).map(str::to_string);
    }
    None
}

/// The pid of the pane a tmux client shows and its session name, asking the
/// client's server.
fn tmux_pane(client_argv: &[String], client: u32) -> Option<(u32, String)> {
    let mut command = Command::new("tmux");
    // The client's own -L/-S picks its server.
    let mut args = client_argv.iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-L" | "-S" => {
                command.args([arg, args.next()?]);
            }
            _ if arg.starts_with("-L") || arg.starts_with("-S") => {
                command.arg(arg);
            }
            _ => {}
        }
    }
    let output = command.args(["list-clients", "-F", "#{client_pid} #{pane_pid} #{session_name}"]).output().ok()?;
    parse_clients(&String::from_utf8_lossy(&output.stdout), client)
}

/// Lines of "<client pid> <pane pid> <session name>"; the name may contain spaces.
fn parse_clients(text: &str, client: u32) -> Option<(u32, String)> {
    text.lines().find_map(|line| {
        let mut fields = line.splitn(3, ' ');
        let pid: u32 = fields.next()?.parse().ok()?;
        let pane: u32 = fields.next()?.parse().ok()?;
        (pid == client).then(|| (pane, fields.next().unwrap_or("").to_string()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake /proc of (pid, ppid, session, tty, tpgid, comm, argv), removed
    /// when dropped.
    struct Fake(Procs);

    impl std::ops::Deref for Fake {
        type Target = Procs;
        fn deref(&self) -> &Procs {
            &self.0
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0.root);
        }
    }

    fn procs(list: &[(u32, u32, u32, u32, i32, &str, &str)]) -> Fake {
        let root = std::env::temp_dir().join(format!("habitd-terminal-{}-{}", std::process::id(), list[0].0));
        for &(pid, ppid, session, tty, tpgid, comm, argv) in list {
            let dir = root.join(pid.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            let pgrp = if tpgid == pid as i32 { pid } else { session };
            std::fs::write(dir.join("stat"), format!("{pid} ({comm}) S {ppid} {pgrp} {session} {tty} {tpgid} 0 0"))
                .unwrap();
            std::fs::write(dir.join("cmdline"), argv.replace(' ', "\0") + "\0").unwrap();
        }
        Fake(Procs::read(&root))
    }

    fn no_tmux(_: &[String], _: u32) -> Option<(u32, String)> {
        None
    }

    fn program(p: &Procs, terminal: u32, title: &str, tmux: TmuxLookup) -> Option<String> {
        p.front_in(terminal, title, tmux).program
    }

    const PTS0: u32 = 136 << 8;

    #[test]
    fn parses_stat_with_odd_names() {
        let p = parse_stat("4423 (tmux: client) S 4033 4423 4033 34816 4423 4194304").unwrap();
        assert_eq!(p, Proc { ppid: 4033, pgrp: 4423, session: 4033, tty: 34816, tpgid: 4423, comm: "tmux: client".into() });
        assert_eq!(parse_stat("7 (a) b) S 1 7 7 0 -1 0").unwrap().comm, "a) b");
    }

    #[test]
    fn finds_the_foreground_program_of_the_shell() {
        let p = procs(&[
            (100, 1, 1, 0, -1, "kitty", "kitty"),
            (101, 100, 1, 0, -1, "kitten", "kitten"),
            (102, 100, 102, PTS0, 103, "bash", "-bash"),
            (103, 102, 102, PTS0, 103, "nvim", "nvim ."),
            (104, 103, 104, 0, -1, "nvim", "nvim --embed ."),
        ]);
        assert_eq!(program(&p, 100, "~", &no_tmux).as_deref(), Some("nvim"));
    }

    #[test]
    fn a_shell_alone_is_no_program() {
        let p = procs(&[(200, 1, 1, 0, -1, "kitty", "kitty"), (201, 200, 201, PTS0, 201, "bash", "/usr/bin/bash")]);
        assert_eq!(program(&p, 200, "~", &no_tmux), None);
    }

    #[test]
    fn follows_tmux_to_the_active_pane() {
        let pane_tty = (136 << 8) | 5;
        let p = procs(&[
            (300, 1, 1, 0, -1, "kitty", "kitty"),
            (301, 300, 301, PTS0, 302, "bash", "-bash"),
            (302, 301, 301, PTS0, 302, "tmux: client", "tmux -L work attach"),
            (310, 1, 310, 0, -1, "tmux: server", "tmux -L work attach"),
            (311, 310, 311, pane_tty, 312, "bash", "-bash"),
            (312, 311, 311, pane_tty, 312, "claude", "claude"),
        ]);
        let tmux = |argv: &[String], client: u32| {
            assert_eq!(argv, ["tmux", "-L", "work", "attach"]);
            (client == 302).then(|| (311, "my thesis".to_string()))
        };
        assert_eq!(
            p.front_in(300, "work", &tmux),
            Front { program: Some("claude".into()), tmux_session: Some("my thesis".into()) }
        );
        // A tmux pane with only a shell still names its session.
        let p = procs(&[
            (320, 1, 1, 0, -1, "kitty", "kitty"),
            (321, 320, 321, PTS0, 322, "bash", "-bash"),
            (322, 321, 321, PTS0, 322, "tmux: client", "tmux"),
            (331, 1, 331, PTS0 | 7, 331, "bash", "-bash"),
        ]);
        let tmux = |_: &[String], _: u32| Some((331, "notes".to_string()));
        assert_eq!(p.front_in(320, "", &tmux), Front { program: None, tmux_session: Some("notes".into()) });
    }

    #[test]
    fn names_interpreted_programs_after_their_script() {
        let argv = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(script_name(&argv("node /usr/lib/node_modules/@openai/codex/bin/codex.js")).as_deref(), Some("codex"));
        assert_eq!(
            script_name(&argv("node --no-warnings /x/node_modules/@anthropic-ai/claude-code/cli.js")).as_deref(),
            Some("claude-code")
        );
        assert_eq!(script_name(&argv("python3 -m aider.main")).as_deref(), Some("aider"));
        assert_eq!(script_name(&argv("node")), None);
        assert!(is_interpreter("python3.13") && is_interpreter("node") && !is_interpreter("nodemon"));
        let p = procs(&[
            (400, 1, 1, 0, -1, "foot", "foot"),
            (401, 400, 401, PTS0, 402, "zsh", "zsh"),
            (402, 401, 401, PTS0, 402, "node", "node /home/u/.local/bin/gemini"),
        ]);
        assert_eq!(program(&p, 400, "", &no_tmux).as_deref(), Some("gemini"));
    }

    #[test]
    fn several_tabs_pick_the_one_in_the_title() {
        let p = procs(&[
            (500, 1, 1, 0, -1, "kitty", "kitty"),
            (501, 500, 501, PTS0, 502, "bash", "bash"),
            (502, 501, 501, PTS0, 502, "nvim", "nvim"),
            (503, 500, 503, PTS0 | 1, 504, "bash", "bash"),
            (504, 503, 503, PTS0 | 1, 504, "btop", "btop"),
        ]);
        assert_eq!(program(&p, 500, "btop", &no_tmux).as_deref(), Some("btop"));
        assert_eq!(program(&p, 500, "NVIM - notes.md", &no_tmux).as_deref(), Some("nvim"));
    }

    #[test]
    fn parses_tmux_clients() {
        assert_eq!(parse_clients("4423 4426 0\n5000 5001 my thesis\n", 5000), Some((5001, "my thesis".into())));
        assert_eq!(parse_clients("4423 4426\n", 1), None);
    }
}
