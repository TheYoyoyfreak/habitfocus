# habitfocus

Habits before distractions. Distracting apps and websites stay blocked until you complete a habit (e.g. 20 minutes of
reading), which earns unlock time. The habit timer only runs while an allowed window is focused and you're not idle;
strict habits pull focus back until you're done. Timer habits cover offline activities like a paper book: they count
while the `hf tui` window is focused. Manual and counter habits (a walk, 50 push-ups) are logged with `hf done`.

Blocks can apply only at certain hours and weekdays, unlock by clock time, by minutes of actual use or for the rest of
the day, and wait for specific habits ("walk *and* work out first"). Unspent credit expires when the day ends.

Working on the code? See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

Shell-agnostic core with thin shell integrations: a [Noctalia plugin](https://github.com/TheYoyoyfreak/habitfocus_noctalia_plugin), and `hf status --waybar` / `hf watch --json
--reconnect` for other bars.

| Component    | Role                                                                    |
|--------------|-------------------------------------------------------------------------|
| `habit-core` | Pure engine: config, sessions, credits, unlocks (unit-tested)            |
| `habit-ipc`  | JSON-lines protocol over `$XDG_RUNTIME_DIR/habitfocus.sock`             |
| `habitd`     | Daemon: niri or Hyprland window events, idle detection (ext-idle-notify), enforcement |
| `hf`         | CLI and TUI (`hf tui`): `status`, `start`, `done`, `unlock`, `events`, `apps`, …; native messaging host |
| `extension/` | Browser extension (Firefox/Zen and Chrome/Brave/Helium): blocked page, tab URLs for habits |

## Install

Needs Linux with niri or Hyprland (for blocking windows) and systemd. The release binaries are static, so any distro
works, on x86_64 and aarch64.

```sh
curl -fsSL https://github.com/TheYoyoyfreak/habitfocus/releases/latest/download/install.sh | sh
```

This puts `habitd` and `hf` in `~/.local/bin`, writes the example config to `~/.config/habitfocus/config.toml`,
installs the systemd user service and registers `hf` with your browsers for the extension. It doesn't start blocking
yet: the example blocks Discord, Steam and some sites. Look through the config first (`hf tui`, then `e` on a
block or habit; app ids come from `niri msg windows` or `hyprctl clients`), then:

```sh
systemctl --user enable --now habitd
```

- **Browser extension** (for blocking websites): from the [latest release](https://github.com/TheYoyoyfreak/habitfocus/releases/latest),
  open `habitfocus-firefox.xpi` in Firefox or Zen (signed; it updates itself). Chrome, Brave and Helium: unzip
  `habitfocus-chromium.zip`, then `chrome://extensions` → Developer mode → *Load unpacked*. See [Web blocking](#web-blocking).
- **Noctalia**: install the [habitfocus plugin](https://github.com/TheYoyoyfreak/habitfocus_noctalia_plugin) for a bar
  widget and panel.
- **Updates**: `hf update` installs the newest release and restarts habitd; `hf tui` and `hf status` say when one is
  out (habitd checks GitHub once a day; turn it off with `update_check = false` or in the TUI settings).
  `hf update --check` only reports (exit code 10 when there's an update).
- **Uninstall**: `curl -fsSL https://github.com/TheYoyoyfreak/habitfocus/releases/latest/download/install.sh | sh -s -- --uninstall`
  (your config and history stay in `~/.config/habitfocus` and `~/.local/state/habitfocus`).

Installer options (after `sh -s --`): `--version v0.2.0` for a specific release, `--prefix DIR` for another
directory, `--from FILE` for a downloaded `habitfocus-<arch>-linux.tar.gz`.

### From source

```sh
cargo install --locked --git https://github.com/TheYoyoyfreak/habitfocus habitd hf   # or --path crates/… in a clone
hf setup            # config, systemd service, browser host (`hf setup --start` also starts habitd)
```

After updating, restart the daemon so it runs the new binary: `systemctl --user restart habitd` (`hf setup --start`
does that too). State is saved on shutdown, so running sessions and credit survive.

## Web blocking

Websites are blocked by the browser extension only; nothing runs as root. It takes effect on navigation, shows a blocked
page with session progress and unlock/start buttons, returns to the site as soon as it is unlocked, and reports tab
URLs for `url` allow rules. While habitd is unreachable it keeps blocking the last known domains. Disabling the
extension disables web blocking. That is a deliberate trade-off: the goal is friction, not a lock you can't open.

1. `hf setup` (the installer runs it) registers `hf` for Firefox, Zen, Chrome, Brave and Helium; so does
   `hf install-native-host` (rerun either after moving the binary, or after installing a new browser).
2. Install the extension from the [latest release](https://github.com/TheYoyoyfreak/habitfocus/releases/latest):
   - **Firefox, Zen**: open `habitfocus-firefox.xpi`. It's signed by Mozilla (not listed in their store) and
     installs permanently; Firefox updates it from the releases by itself.
   - **Chrome/Brave/Helium**: unzip `habitfocus-chromium.zip`, `chrome://extensions` → Developer mode →
     *Load unpacked* → the unzipped folder.
   - **From a clone**: `contrib/package-extension.sh` builds an unsigned `dist/habitfocus.xpi` (Zen and other builds
     with `xpinstall.signatures.required = false` in `about:config`), or load `extension/manifest.json` temporarily in
     Firefox via `about:debugging`; Chromium browsers load `extension/` unpacked.

The blocked page lists what the block waits for and unlocks with the group's unlock mode. Habits you log by hand
can't be logged from a web page; it shows the `hf done` command instead.

`tests/extension/run.sh` runs end-to-end checks in a headless Firefox-based browser (`BROWSER=/usr/bin/firefox` to
pick one): blocking, no navigation churn while a session runs, the required-habit gate, unlocking back to a slow
site, and relocking an open tab.

## Usage

```sh
hf tui                    # dashboard: today, blocks, habits, insights, commitment lock
hf start reading          # timer runs only while zathura/foliate is focused
hf done journal           # log a manual habit; `hf done pushups 25` adds to a counter
hf stop                   # stop and keep the progress; `hf start reading` resumes it today
hf continue               # keep going after a round (rewards are banked per finished round)
hf stats                  # streaks and a 4-week grid per habit
hf events                 # activity log: sessions, rewards, unlocks, blocks, expired credit
hf apps                   # screen time per app and site: opens, average session, time of day
hf apps --app youtube     # one app or site (all matching hosts summed), --days 30 for a month
hf rename steam_app_275850 "No Man's Sky"   # a readable name for an app id or site key
hf category steam_app_275850 Games          # put it in a category
hf apps --by-category     # screen time per category; --app games filters by app, site or category
hf lock 7d                # commitment: no easier rules for a week (see below)
hf set day_start 04:00    # settings: day_start, idle_timeout, emergency_penalty, expiry_warning,
                          #           notifications, sound
hf set sound bell         # sound when a timed habit is done ("" for silence)
hf status                 # human-readable; --json for scripts, --waybar for Waybar
hf watch --json           # snapshot stream for shell plugins
hf unlock social 30m      # spend earned credit (omit duration to spend all)
hf relock social          # end early, unused time is refunded
hf abort                  # discard the progress (soft habits only)
hf abort --emergency      # strict habits; unlocks are refused for a while afterwards
```

In a strict session every other window is refocused away, so run `hf abort --emergency` from a keybind (niri
`spawn "hf" "abort" "--emergency"`, Hyprland `bind = SUPER SHIFT, Escape, exec, hf abort --emergency`) or add your terminal to `strict_exempt_apps`.

The TUI has five panes, switched with `1`–`5`, `tab`/`shift-tab` or `h`/`l`. From 100 columns on they're listed in
a sidebar with today's numbers, below that in a tab bar. The footer shows the keys of the current pane.

| Pane         | Shows                                                               | Keys                                  |
|--------------|---------------------------------------------------------------------|---------------------------------------|
| 1 Today      | blocked/credit/streak tiles, the session, habit progress, blocks, log | `enter` start or log, `+`/`-`, `=` amount |
| 2 Blocks     | every group: apps and sites, what unlocks it, rule, state; details    | `enter` spend all, `u` choose, `r` relock, `e` edit, `n` new, `D` delete |
| 3 Habits     | kind, today, streaks, reward; a 20-week heatmap of the selected habit | as in Today, plus `e` edit, `n` new, `D` delete |
| 4 Insights   | screen time, opens and average session per app, site or category; hourly chart stacked by habit, category or app; the week in numbers | `j`/`k` choose, `f` filter, `g` by category, `c` category, `r` rename, `[`/`]` earlier/later day, `d` day / 7 days |
| 5 Lock       | the commitment lock, held-back changes, what it forbids               | `enter` commit/extend, `e` end early  |

Everywhere: `s` stops and keeps progress, `c` continues after a round, `a` discards a soft session, `A`
emergency-aborts, `o` opens settings, `R` reloads the config, `L` jumps to the lock, `q` quits.

### Editing blocks and habits in the TUI

`e` on a block (Blocks pane) or habit (Habits pane) opens a form with its settings; `n` starts a new one, `D` deletes
one. Move with `j`/`k`, `enter` edits a value, `h`/`l` (or space) changes a choice, space ticks habits or blocks,
`a` adds an app or site (picked from what you used recently, or typed) and `d` removes one. The right column shows
what will be written to `config.toml`; `ctrl+s` saves and applies it right away, `esc` closes.

A habit's **Counts in** list holds its allow rules. `a` suggests what you used recently (apps, sites, terminal
programs) and your tmux sessions, or takes typed words: an app id (`zed`), `site:github.com` (the site and its
subdomains), `term:nvim`, `tmux:thesis`, or `title:`/`url:` with a regex. Several words make one rule that needs all
of them, `kitty tmux:work`; separate rules are alternatives. An apps habit without rules counts any window.

habitd writes the file for you and only touches the lines you changed, so your comments stay. Schedules and processes
are shown but still edited in `config.toml`. During a commitment lock the same rules apply as for
editing the file: changes that make things easier are refused with the reason, stricter ones apply.

### Blocks

A group (`[groups.<id>]` in the config) blocks its apps, processes and sites. By default it blocks around the clock and
an unlock runs on the clock. Each of these can change per group:

- **Schedule**: `schedule = { days = ["mon", "tue"], ranges = ["09:00-17:00"] }` (or a list of those) blocks only
  inside the windows. Ranges can run past midnight. Outside them nothing is blocked and unlocking is refused, so no
  credit is wasted.
- **Unlock mode**: `"wallclock"` (default) runs on the clock; `"usage"` is a budget of actual use that only runs down
  while one of the group's apps or sites is focused; `"rest_of_day"` is a day pass for `rest_of_day_price`.
- **Required habits**: `requires = ["walk", "workout"]` refuses unlocks until one of them (`require = "any"`, the
  default) or all of them (`"all"`) are done today. An immediate reward for a group that's still waiting is banked as
  credit instead.

Credit is earned per group and expires when a new day starts (`day_start`): each day starts from zero.

### Habits you log

`kind = "manual"` habits are done or not (`hf done journal`) and pay once a day. `kind = "counter"` habits count
toward a `goal` in a `unit` and pay one reward per full goal, up to `daily_limit` rewards a day:

```toml
[habits.pushups]
kind = "counter"
goal = 50
unit = "reps"
daily_limit = 2
reward = { groups = ["games"], duration = "20m" }
```

`hf done pushups 30` adds 30, `hf done pushups -5` corrects a typo and `hf done pushups 40 --set` sets today's count.
Corrections never take back rewards already earned. In the TUI, `enter` or `+` logs one, `-` takes one back and `=`
asks for an amount. The Noctalia panel has a *+1* / *Done* button.

### Passive habits

`kind = "passive"` habits count by themselves, like a step counter: no session to start, their time adds up all day
while a window matching `allow` is focused and you're not idle. The rules are the same as for apps habits (`app`,
`title`, `url`, `program`, `tmux_session`), and passive habits count alongside a running session.

```toml
[habits.coding]
name = "Coding"
kind = "passive"
target = "2h"                    # optional daily goal
allow = [{ app = "zed" }, { program = "nvim" }, { tmux_session = "work" }]
reward = { groups = ["social"], duration = "30m" }   # optional

[habits.agents]
name = "AI agents"
kind = "passive"                 # no target: only tracked
allow = [{ program = "claude" }, { program = "codex" }]
```

With a `target`, reaching it completes the habit for the day (a notification and the sound, streaks, and it counts
for groups that `requires` it) and pays the reward, if it has one; `daily_limit` lets every further full target
pay again. Without a target the habit is only tracked: the TUI, `hf status` and the panel show today's time, and the
Habits pane its weeks. Passive habits can be made entirely in the TUI form (`n` in the Habits pane), rules included;
clear the target to only track.

### Screen time and history

habitd counts time per app, and per site where the browser extension reports tabs, skipping idle time. Daily totals
live in `state.json` (the last `screen_time_days`, default 60). Everything that only grows goes to
`~/.local/state/habitfocus/history.db`, an SQLite database kept without limit:

- **visits**: every stretch of focus on an app or site, with start, end and active time. From these come how often
  you open something (visits less than a minute apart count as one), the average and longest session, and use per
  hour of the day. They're recorded from the version that introduced the database on; totals from before stay.
- **events**: the full activity log (`hf events --limit 1000` reaches back as far as you like).
- **sessions**: every habit session.

Some apps report ids that say little, like `steam_app_275850` for a Steam game. `r` in the Insights pane (or
`hf rename <id> "<name>"`) gives one a name, stored under `[app_names]` in `config.toml` and shown in Insights,
`hf apps`, the Blocks pane and notifications. `c` (or `hf category <id> <category>`) puts an app or site in a
category (`[app_categories]`); `g` in Insights and `hf apps --by-category` then total per category. Names and
categories are only labels; blocking still matches the id.

**Programs in terminals** count on their own: time in a terminal goes to what runs in front, like `term:nvim` or
`term:claude`, shown as "Neovim" or "Claude Code" (the terminal itself, e.g. `kitty`, is what's left: an idle
shell). habitd looks at the terminal shell's foreground process every 2 seconds, and follows tmux to the pane the
client shows. Programs run by node or python are named after their script. With several tabs in one terminal
process, the program named in the window title wins, else the tab typed into last. Terminals are listed in
`general.terminals` (kitty, Alacritty, foot, Ghostty, WezTerm and a few more by default); screen, zellij and ssh
show up as themselves. Turn it off with `terminal_programs = false` or in the TUI settings (`o`).

A habit can count only while a program is in front: `allow = [{ app = "kitty", program = "nvim" }]` runs the timer
while nvim is the program in the focused kitty window, including in the tmux pane you're on, and pauses when you
switch to another pane or window. Without `app` any terminal counts. `program` is the name `hf apps` shows after
`term:`. The rule works even with `terminal_programs = false`; screen time then just stays with the terminal.

`tmux_session` does the same for a named tmux session: `allow = [{ tmux_session = "thesis" }]` counts while the
focused terminal shows the session `thesis` (`tmux new -s thesis`), whatever runs in it, and pauses in any other
session or outside tmux. Names are exact, including case. Combined with `program`, both must match:
`{ tmux_session = "thesis", program = "nvim" }`.

Terminals and their programs are in the category **Terminal**, browsers (`general.browsers`) and sites in
**Browser**, so `g` totals them. `[app_categories]` overrides this per key; `auto_categories = false` (or the TUI
settings) turns it off.

The **hourly chart** under the table shows the time on screen per hour of the day. On the "All" row the bars are
stacked and colored by what the time went to, with a legend below; pick an app, site or category with `j`/`k` and
the chart shows only that one's hours. `[` and `]` step to an earlier or later day (the title says "Today",
"Yesterday" or the date, and stepping stops where the recording begins), `d` switches between one day and the whole
period.

Time while a habit's session runs counts as that habit (marked `✓` in the legend), not as the app it happened in:
twenty minutes of reading in zathura read as "Reading", the rest of zathura stays "zathura". Everything else counts
as its category, or as the app or site when it has none. The per-app view is the app's own screen time, so a habit's
time still counts there for the app it ran in.

`f` in Insights filters like btop: type and the table narrows to apps, sites or categories containing the text, and
the total and the chart follow. `enter` keeps the filter, `esc` clears it.

Only habitd writes the database; `hf` asks it through the socket. It's plain SQLite, so
`sqlite3 ~/.local/state/habitfocus/history.db` works for your own queries.

### The sound when a habit is done

When a timed habit reaches its target — and at every round of a timer habit — habitd plays a short sound. Set it
with `sound` in `[general]`, with `hf set sound …` or in the TUI settings (`o`):

```toml
[general]
sound = "complete"          # a name from the system's sound theme: complete, bell, message, …
# sound = "~/sounds/gong.ogg"   # or a file
# sound = ""                    # or silence
```

Names are looked up in the usual sound directories (`~/.local/share/sounds`, `/usr/share/sounds/…`), and the first
player found is used (`pw-play`, `paplay`, `ffplay`, `mpv`, `aplay`, or `canberra-gtk-play`). Habits you log by hand
(`hf done`) make no sound.

### Streaks

A day counts for a habit when it was completed at least once (days follow `day_start`). The current streak stays
alive until today ends. Daily totals are kept separately from the capped session history, so long streaks stay
accurate. `hf stats` and the TUI Habits pane show current and best streaks, and a heatmap of each day.

### Commitment lock

`hf lock 7d` (or `hf lock --until 2026-10-01`, or `L` in the TUI) commits you to your current rules:

- Config changes that make things easier (lower targets, bigger rewards, removed apps or domains, soft instead of
  strict, a shorter emergency penalty, …) are held back until the lock ends; `hf reload` lists them and they apply
  automatically afterwards. Stricter changes apply right away. New timed habits are fine unless they pay more per
  minute than your existing ones; new habits you log by hand always wait. Shrinking a schedule, an easier unlock mode,
  dropping a required habit or `all` → `any`, and a lower goal or higher daily limit count as easier too.
- The lock can be extended, not shortened. `hf lock end` ends it 24 hours later; `hf lock cancel-end` keeps it.
- Stopping habitd doesn't help: when it starts again the lock is extended by the downtime. Reboots, suspend and
  logging out don't count.
- Windows of the browsers in `general.browsers` are closed while the extension isn't running in them.

`hf lock status` shows the remaining time and the held-back changes.

### Timer habits

`kind = "timer"` habits count while the `hf tui` window is focused, using the terminal's focus reporting, and ignore
keyboard/mouse inactivity. Works in kitty, foot, alacritty, ghostty and wezterm; inside tmux it needs
`set -g focus-events on`, and the TUI refuses to count without it.

Timer habits (and anything with `on_target = "ask"`) work in **rounds**: every full target banks the reward and the
clock starts over, so a 30 minute target with a 1 hour reward pays 1 hour at 30:00, another at 60:00, and so on.
Nothing is earned in between. `hf continue` (or `c` in the TUI) keeps going after a round, `hf stop` ends the session
— finished rounds stay banked and the unfinished part is saved as today's progress.

Progress of stopped sessions resets when a new day starts (`day_start`, default midnight, changeable in the TUI
settings with `o`).

## Status

- [x] Engine, niri and Hyprland adapters, idle detection, app/process blocking, soft/strict sessions, CLI
- [x] Web blocking: browser extension
- [x] TUI
- [x] Noctalia v5 plugin
- [x] Commitment lock
- [x] History and streaks
- [x] Schedules, unlock modes, daily credit expiry, required habits
- [x] Manual and counter habits, activity log, screen time
- [ ] Anki integration
