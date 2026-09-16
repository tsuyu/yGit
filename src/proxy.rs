//! A stand-in for the `ssh` client, used as git's transport.
//!
//! `git clone` runs without a terminal, so OpenSSH can never ask for a
//! password. When the profile authenticates with one, the app points
//! `GIT_SSH_COMMAND` at itself (`ygit --ssh-proxy`) instead: this module speaks
//! the command line git expects, opens the session with `russh` using the
//! password the app already holds, and pipes git's stdin and stdout through the
//! SSH channel.

use std::io::{Read, Write};

use anyhow::{anyhow, Result};
use russh::ChannelMsg;
use tokio::sync::mpsc::unbounded_channel;

use crate::config::{AuthKind, Profile};
use crate::ssh::{self, Evt};

/// Flag that turns this process into the git transport.
pub const FLAG: &str = "--ssh-proxy";

/// Environment variables carrying the connection details to the child.
pub const ENV_HOST: &str = "YGIT_PROXY_HOST";
pub const ENV_PORT: &str = "YGIT_PROXY_PORT";
pub const ENV_USER: &str = "YGIT_PROXY_USER";
pub const ENV_STRICT: &str = "YGIT_PROXY_STRICT";
/// The session password. It lives in the child's environment only, which on
/// the supported platforms this user alone can read.
pub const ENV_PASSWORD: &str = "YGIT_PROXY_PASSWORD";

/// ssh options that take a separate value, so the value is never mistaken for
/// the host name.
const FLAGS_WITH_VALUE: &[&str] = &[
    "-B", "-b", "-c", "-D", "-E", "-e", "-F", "-I", "-i", "-J", "-L", "-l", "-m", "-O", "-o", "-p",
    "-Q", "-R", "-S", "-W", "-w",
];

/// The host and remote command git asked for.
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub user: Option<String>,
    pub host: String,
    pub command: String,
}

/// Parses the ssh-style command line git hands to the transport.
pub fn parse_args(args: &[String]) -> Result<Request> {
    let mut rest = args.iter();
    let mut target: Option<String> = None;
    let mut command: Vec<String> = Vec::new();

    while let Some(a) = rest.next() {
        if target.is_none() && a.starts_with('-') {
            if FLAGS_WITH_VALUE.contains(&a.as_str()) {
                // `-p 2222`; the glued form `-p2222` carries its own value.
                let _ = rest.next();
            }
            continue;
        }
        match target {
            None => target = Some(a.clone()),
            Some(_) => command.push(a.clone()),
        }
    }

    let target = target.ok_or_else(|| anyhow!("no host in the ssh arguments"))?;
    if command.is_empty() {
        return Err(anyhow!("no remote command in the ssh arguments"));
    }
    let (user, host) = match target.split_once('@') {
        Some((u, h)) => (Some(u.to_string()), h.to_string()),
        None => (None, target),
    };
    Ok(Request {
        user,
        host,
        command: command.join(" "),
    })
}

/// Builds the profile for the proxy from the environment and git's arguments.
fn profile_from_env(req: &Request) -> Result<(Profile, String)> {
    let password = std::env::var(ENV_PASSWORD)
        .map_err(|_| anyhow!("{ENV_PASSWORD} is not set; start the clone from the app"))?;
    let host = std::env::var(ENV_HOST).unwrap_or_else(|_| req.host.clone());
    let user = std::env::var(ENV_USER)
        .ok()
        .or_else(|| req.user.clone())
        .ok_or_else(|| anyhow!("no user for the connection"))?;
    let port: u16 = std::env::var(ENV_PORT)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(22);
    let strict = std::env::var(ENV_STRICT).map(|v| v == "1").unwrap_or(false);

    Ok((
        Profile {
            host,
            port,
            user,
            auth: AuthKind::Password,
            strict_host_key: strict,
            ..Default::default()
        },
        password,
    ))
}

/// Runs the transport. The return value is the exit code for git.
pub fn run(args: &[String]) -> i32 {
    match try_run(args) {
        Ok(code) => code,
        Err(e) => {
            // git shows the transport's stderr verbatim.
            let _ = writeln!(std::io::stderr(), "ygit ssh proxy: {e:#}");
            255
        }
    }
}

fn try_run(args: &[String]) -> Result<i32> {
    let req = parse_args(args)?;
    let (profile, password) = profile_from_env(&req)?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(session(profile, password, req.command))
}

async fn session(profile: Profile, password: String, command: String) -> Result<i32> {
    // Connection progress belongs on stderr, which git passes through.
    let (log_tx, mut log_rx) = unbounded_channel::<Evt>();
    tokio::spawn(async move {
        while let Some(e) = log_rx.recv().await {
            if let Evt::Log(line) = e {
                let _ = writeln!(std::io::stderr(), "ygit: {line}");
            }
        }
    });

    let handle = ssh::connect(&profile, &password, "", &log_tx).await?;
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, command.as_bytes()).await?;

    // stdin is a blocking pipe from git, so it gets its own thread.
    let (in_tx, mut in_rx) = unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if in_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let mut code = 0i32;
    let mut sent_eof = false;

    loop {
        tokio::select! {
            data = in_rx.recv(), if !sent_eof => match data {
                Some(bytes) => channel.data(&bytes[..]).await?,
                None => {
                    sent_eof = true;
                    channel.eof().await?;
                }
            },
            msg = channel.wait() => match msg {
                Some(ChannelMsg::Data { ref data }) => {
                    stdout.write_all(data)?;
                    stdout.flush()?;
                }
                Some(ChannelMsg::ExtendedData { ref data, .. }) => {
                    stderr.write_all(data)?;
                    stderr.flush()?;
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => code = exit_status as i32,
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                Some(_) => {}
            },
        }
    }

    stdout.flush()?;
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn option_values_are_not_taken_for_the_host() {
        let r = parse_args(&args(&[
            "-p",
            "2222",
            "-o",
            "SendEnv=GIT_PROTOCOL",
            "nas.local",
            "git-upload-pack '/volume1/git/p.git'",
        ]))
        .unwrap();
        assert_eq!(r.host, "nas.local");
        assert_eq!(r.user, None);
        assert_eq!(r.command, "git-upload-pack '/volume1/git/p.git'");
    }

    #[test]
    fn user_is_split_off_the_host() {
        let r = parse_args(&args(&["me@nas.local", "git-upload-pack", "/p.git"])).unwrap();
        assert_eq!(r.user.as_deref(), Some("me"));
        assert_eq!(r.host, "nas.local");
        // A command split over several arguments is rejoined.
        assert_eq!(r.command, "git-upload-pack /p.git");
    }

    #[test]
    fn a_missing_host_or_command_is_an_error() {
        assert!(parse_args(&args(&[])).is_err());
        assert!(parse_args(&args(&["nas.local"])).is_err());
    }
}
