use secrecy::Secret;
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use xmpp_parsers::jid::FullJid;

/// Uniquely identify an account inside Aparté
pub type Account = FullJid;

pub type Password = Secret<String>;

// Long enough to answer a pinentry; the UI is blocked while the command runs.
const PASSWORD_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

fn false_() -> bool {
    false
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ConnectionInfo {
    pub jid: String,
    pub server: Option<String>,
    pub port: Option<u16>,
    #[serde(default = "false_")]
    pub autoconnect: bool,
    #[serde(skip_serializing)]
    pub password: Option<Password>,
    /// Program and arguments, run without a shell. The first line of stdout is the password.
    pub password_command: Option<Vec<String>>,
}

impl ConnectionInfo {
    pub fn resolve_password(&self) -> Option<Password> {
        self.password.clone().or_else(|| {
            self.password_command
                .as_deref()
                .and_then(|argv| run_password_command(argv, PASSWORD_COMMAND_TIMEOUT))
        })
    }
}

fn run_password_command(argv: &[String], timeout: Duration) -> Option<Password> {
    let (program, args) = argv.split_first()?;
    // A terminal pinentry would draw over the TUI.
    let mut child = Command::new(program)
        .args(args)
        .env_remove("GPG_TTY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| log::warn!("Cannot run password command {program}: {err}"))
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (output_tx, output_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut output = Vec::new();
        let result = stdout.read_to_end(&mut output).map(|_| output);
        let _ = output_tx.send(result);
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                log::warn!("Password command {program} timed out");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Err(err) => {
                log::warn!("Cannot wait for password command {program}: {err}");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        log::warn!("Password command {program} failed: {status}");
        return None;
    }

    // A daemon started by the command can keep stdout open after it exits.
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(100));
    let output = output_rx.recv_timeout(remaining).ok()?.ok()?;
    let mut password = String::from_utf8(output).ok()?;
    if let Some(end) = password.find('\n') {
        password.truncate(end);
    }
    if password.ends_with('\r') {
        password.pop();
    }
    if password.is_empty() {
        log::warn!("Password command {program} returned no password");
        return None;
    }
    Some(Secret::new(password))
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn shell(script: &str) -> Vec<String> {
        vec!["sh".to_string(), "-c".to_string(), script.to_string()]
    }

    fn secret(password: Option<Password>) -> Option<String> {
        password.map(|password| password.expose_secret().clone())
    }

    #[test]
    fn uses_first_line_of_output() {
        let argv = shell("printf 'hunter2\\nurl: example\\n'");
        assert_eq!(
            secret(run_password_command(&argv, TIMEOUT)),
            Some("hunter2".to_string())
        );
    }

    #[test]
    fn keeps_surrounding_spaces() {
        let argv = shell("printf ' spaced pass \\r\\n'");
        assert_eq!(
            secret(run_password_command(&argv, TIMEOUT)),
            Some(" spaced pass ".to_string())
        );
    }

    #[test]
    fn failing_command_gives_no_password() {
        let argv = shell("echo partial; exit 1");
        assert!(run_password_command(&argv, TIMEOUT).is_none());
    }

    #[test]
    fn empty_output_gives_no_password() {
        assert!(run_password_command(&shell("true"), TIMEOUT).is_none());
    }

    #[test]
    fn missing_program_gives_no_password() {
        let argv = vec!["/nonexistent/password-command".to_string()];
        assert!(run_password_command(&argv, TIMEOUT).is_none());
    }

    #[test]
    fn empty_command_gives_no_password() {
        assert!(run_password_command(&[], TIMEOUT).is_none());
    }

    #[test]
    fn hanging_command_times_out() {
        let before = Instant::now();
        let argv = shell("exec sleep 30");
        assert!(run_password_command(&argv, Duration::from_millis(100)).is_none());
        assert!(before.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn background_process_holding_stdout_does_not_hang() {
        let before = Instant::now();
        let argv = shell("sleep 30 & echo never-read");
        let _ = run_password_command(&argv, Duration::from_millis(200));
        assert!(before.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn stored_password_takes_precedence() {
        let info = ConnectionInfo {
            password: Some(Secret::new("stored".to_string())),
            password_command: Some(shell("echo command")),
            ..ConnectionInfo::default()
        };
        assert_eq!(secret(info.resolve_password()), Some("stored".to_string()));
    }

    #[test]
    fn password_command_is_read_from_config() {
        let info: ConnectionInfo = toml::from_str(
            "jid = \"me@example.org\"\npassword_command = [\"sh\", \"-c\", \"echo fromconfig\"]\n",
        )
        .unwrap();
        assert_eq!(
            secret(info.resolve_password()),
            Some("fromconfig".to_string())
        );
    }
}
