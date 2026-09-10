use std::process::Command;

use anyhow::{anyhow, Result};

use crate::config::Profile;

/// Builds the argument list for the system `ssh` client.
/// When `cd` is given the shell starts inside that directory.
fn ssh_args(profile: &Profile, cd: Option<&str>) -> Vec<String> {
    let mut args = vec!["-p".to_string(), profile.port.to_string()];
    if !profile.key_path.trim().is_empty() {
        args.push("-i".to_string());
        args.push(profile.key_path.trim().to_string());
    }
    if cd.is_some() {
        args.push("-t".to_string());
    }
    args.push(format!("{}@{}", profile.user, profile.host));
    if let Some(dir) = cd {
        let quoted = dir.replace('\'', "'\\''");
        args.push(format!(
            "cd '{quoted}' && exec ${{SHELL:-/bin/sh}} -l || exec /bin/sh -l"
        ));
    }
    args
}

/// Quotes one argument for a Windows command line.
#[cfg(windows)]
fn win_quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"', '&', '|', '<', '>', '^']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                out.push_str(&"\\".repeat(backslashes + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                backslashes = 0;
                out.push(c);
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes));
    out.push('"');
    out
}

/// Opens an interactive SSH session in a new terminal window.
pub fn open_ssh(profile: &Profile, cd: Option<&str>) -> Result<String> {
    if profile.host.trim().is_empty() || profile.user.trim().is_empty() {
        return Err(anyhow!("set user and host before opening a terminal"));
    }
    let args = ssh_args(profile, cd);
    launch(&args)
}

#[cfg(windows)]
fn launch(args: &[String]) -> Result<String> {
    use std::os::windows::process::CommandExt;

    let quoted: Vec<String> = args.iter().map(|a| win_quote(a)).collect();
    let joined = quoted.join(" ");

    // Windows Terminal keeps the session in a tab; fall back to a console window.
    if Command::new("wt.exe")
        .arg("new-tab")
        .arg("ssh")
        .args(args)
        .spawn()
        .is_ok()
    {
        return Ok(format!("wt.exe new-tab ssh {joined}"));
    }

    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    Command::new("ssh")
        .args(args)
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
        .map_err(|e| anyhow!("could not start ssh: {e}"))?;
    Ok(format!("ssh {joined}"))
}

#[cfg(target_os = "macos")]
fn launch(args: &[String]) -> Result<String> {
    let cmd = shell_join(args);
    let script = format!("tell application \"Terminal\" to do script \"ssh {cmd}\"");
    Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .spawn()
        .map_err(|e| anyhow!("could not start Terminal: {e}"))?;
    Ok(format!("ssh {cmd}"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn launch(args: &[String]) -> Result<String> {
    let cmd = shell_join(args);
    let full = format!("ssh {cmd}");
    for term in [
        "x-terminal-emulator",
        "gnome-terminal",
        "konsole",
        "alacritty",
        "kitty",
        "xterm",
    ] {
        let spawned = match term {
            "gnome-terminal" => Command::new(term).arg("--").arg("sh").arg("-c").arg(&full).spawn(),
            _ => Command::new(term).arg("-e").arg("sh").arg("-c").arg(&full).spawn(),
        };
        if spawned.is_ok() {
            return Ok(full);
        }
    }
    Err(anyhow!("no terminal emulator found; run manually: {full}"))
}

#[cfg(unix)]
fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.chars().all(|c| c.is_alphanumeric() || "@_-.:/".contains(c)) {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The clone URL for a repository living at `path` on the server.
pub fn clone_url(profile: &Profile, path: &str) -> String {
    if profile.port == 22 {
        format!("ssh://{}@{}{}", profile.user, profile.host, path)
    } else {
        format!(
            "ssh://{}@{}:{}{}",
            profile.user, profile.host, profile.port, path
        )
    }
}
