//! `hf sync`: multi-device sync. habitd does the work; this asks for the
//! password and key and passes them on.

use anyhow::{bail, Context};
use clap::Subcommand;
use habit_ipc::{Request, Response};
use std::io::{BufRead, IsTerminal};

#[derive(Subcommand)]
pub enum SyncCommand {
    /// Create an account on a sync server and sign this device in
    Register {
        /// e.g. https://sync.example.org
        server: String,
        username: String,
        /// Defaults to <username>@habitfocus.invalid
        #[arg(long)]
        email: Option<String>,
    },
    /// Sign this device in to an existing account (asks for the password and sync key)
    Login { server: String, username: String },
    /// Sign this device out; what it got from the other devices stays
    Logout,
    /// Show the account, the other devices and the last sync
    Status,
    /// Sync right away
    Now,
    /// Print the sync key, for signing in another device
    Key,
}

pub fn run(command: SyncCommand) -> anyhow::Result<()> {
    let request = match command {
        SyncCommand::Register { server, username, email } => {
            let password = read_secret("Password: ")?;
            if std::io::stdin().is_terminal() && read_secret("Repeat the password: ")? != password {
                bail!("the passwords don't match");
            }
            let email = email.unwrap_or_else(|| format!("{username}@habitfocus.invalid"));
            eprintln!("Registering…");
            Request::SyncRegister { server, username, email, password }
        }
        SyncCommand::Login { server, username } => {
            let password = read_secret("Password: ")?;
            let key = read_secret("Sync key (hfk1-…, from `hf sync key` on a signed-in device): ")?;
            eprintln!("Signing in…");
            Request::SyncLogin { server, username, password, key }
        }
        SyncCommand::Logout => Request::SyncLogout,
        SyncCommand::Status => Request::SyncStatus,
        SyncCommand::Now => {
            eprintln!("Syncing…");
            Request::SyncNow
        }
        SyncCommand::Key => Request::SyncKey,
    };
    let response: Response = habit_ipc::request(&request).map_err(anyhow::Error::msg)?;
    if !response.ok {
        bail!("{}", response.error.unwrap_or_else(|| "request failed".into()));
    }
    if let Some(message) = response.message {
        println!("{message}");
    }
    Ok(())
}

/// Asks without echoing in a terminal; otherwise reads a line from stdin.
fn read_secret(prompt: &str) -> anyhow::Result<String> {
    let secret = if std::io::stdin().is_terminal() {
        rpassword::prompt_password(prompt).context("can't read from the terminal")?
    } else {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        line.trim_end_matches(['\n', '\r']).to_string()
    };
    if secret.is_empty() {
        bail!("nothing entered");
    }
    Ok(secret)
}
