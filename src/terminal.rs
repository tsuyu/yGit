use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Result};

use crate::config::{AuthKind, Profile};
use crate::proxy;

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
    launch("ssh", &args)
}

#[cfg(windows)]
fn launch(program: &str, args: &[String]) -> Result<String> {
    use std::os::windows::process::CommandExt;

    let quoted: Vec<String> = args.iter().map(|a| win_quote(a)).collect();
    let joined = quoted.join(" ");

    // Windows Terminal keeps the session in a tab; fall back to a console window.
    if Command::new("wt.exe")
        .arg("new-tab")
        .arg(program)
        .args(args)
        .spawn()
        .is_ok()
    {
        return Ok(format!("wt.exe new-tab {program} {joined}"));
    }

    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    Command::new(program)
        .args(args)
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
        .map_err(|e| anyhow!("could not start {program}: {e}"))?;
    Ok(format!("{program} {joined}"))
}

#[cfg(target_os = "macos")]
fn launch(program: &str, args: &[String]) -> Result<String> {
    let cmd = format!("{program} {}", shell_join(args));
    let script = format!("tell application \"Terminal\" to do script \"{cmd}\"");
    Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .spawn()
        .map_err(|e| anyhow!("could not start Terminal: {e}"))?;
    Ok(cmd)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn launch(program: &str, args: &[String]) -> Result<String> {
    let full = format!("{program} {}", shell_join(args));
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

/// The directory name `git clone` would create for a repository path.
pub fn clone_dir_name(repo_path: &str) -> String {
    let path = repo_path.trim().trim_end_matches('/');
    let base = path.rsplit('/').next().unwrap_or(path);
    let base = base.strip_suffix("/.git").unwrap_or(base);
    if base == ".git" {
        // A working copy: name the clone after the project directory.
        let parent = path.trim_end_matches("/.git");
        return parent
            .rsplit('/')
            .next()
            .unwrap_or("repo")
            .trim_end_matches(".git")
            .to_string();
    }
    base.trim_end_matches(".git").to_string()
}

/// Quotes one word for the POSIX shell git uses to split `GIT_SSH_COMMAND`.
fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || "@_-.:/=".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\''"))
}

/// Builds the `GIT_SSH_COMMAND` for a key profile: the system `ssh` with the
/// profile's port, key and host key policy.
///
/// `BatchMode=yes` keeps git from hanging on a prompt no one can answer: the
/// clone runs without a terminal, so a key passphrase could never be typed.
/// Password profiles do not come through here at all; they use the app's own
/// transport (see [`proxy`](crate::proxy)).
pub fn git_ssh_command(profile: &Profile) -> String {
    let mut parts = vec![
        "ssh".to_string(),
        "-p".to_string(),
        profile.port.to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
    ];
    let key = profile.key_path.trim();
    if !key.is_empty() {
        parts.push("-i".to_string());
        parts.push(key.to_string());
        // Do not let an agent key shadow the one the profile names.
        parts.push("-o".to_string());
        parts.push("IdentitiesOnly=yes".to_string());
    }
    parts.push("-o".to_string());
    parts.push(
        if profile.strict_host_key {
            "StrictHostKeyChecking=yes"
        } else {
            "StrictHostKeyChecking=accept-new"
        }
        .to_string(),
    );
    parts.iter().map(|p| sh_quote(p)).collect::<Vec<_>>().join(" ")
}

/// Where a clone of `repo_path` would land under `parent`, plus its arguments.
///
/// An empty `branch` leaves the choice to the server's `HEAD`.
pub fn clone_args(
    repo_path: &str,
    url: &str,
    parent: &Path,
    folder: &str,
    branch: &str,
) -> Result<(PathBuf, Vec<String>)> {
    let folder = folder.trim();
    let folder = if folder.is_empty() {
        clone_dir_name(repo_path)
    } else {
        folder.to_string()
    };
    if folder.is_empty()
        || folder == "."
        || folder == ".."
        || folder.contains(['/', '\\'])
    {
        return Err(anyhow!("'{folder}' is not a valid folder name"));
    }
    if parent.as_os_str().is_empty() {
        return Err(anyhow!("choose a destination folder first"));
    }
    let dest = parent.join(&folder);
    let mut args = vec!["clone".to_string(), "--progress".to_string()];
    let branch = branch.trim();
    if !branch.is_empty() {
        if branch.starts_with('-') {
            return Err(anyhow!("'{branch}' is not a valid branch name"));
        }
        args.push("--branch".to_string());
        args.push(branch.to_string());
    }
    args.push(url.to_string());
    args.push(dest.to_string_lossy().to_string());
    Ok((dest.clone(), args))
}

/// The transport git should use for this profile.
///
/// A key profile uses the system `ssh`. A password profile uses this very
/// binary in `--ssh-proxy` mode, which authenticates with `russh` and the
/// password the app already holds, because `ssh` has no terminal to ask on.
fn transport(profile: &Profile, password: &str) -> Result<(String, Vec<(String, String)>)> {
    if profile.auth != AuthKind::Password {
        return Ok((git_ssh_command(profile), Vec::new()));
    }
    if password.is_empty() {
        return Err(anyhow!(
            "this profile logs in with a password; connect first so the clone can reuse it"
        ));
    }
    let exe = std::env::current_exe().map_err(|e| anyhow!("cannot locate ygit itself: {e}"))?;
    let cmd = format!("{} {}", sh_quote(&exe.to_string_lossy()), proxy::FLAG);
    let env = vec![
        (proxy::ENV_HOST.to_string(), profile.host.trim().to_string()),
        (proxy::ENV_PORT.to_string(), profile.port.to_string()),
        (proxy::ENV_USER.to_string(), profile.user.trim().to_string()),
        (
            proxy::ENV_STRICT.to_string(),
            if profile.strict_host_key { "1" } else { "0" }.to_string(),
        ),
        (proxy::ENV_PASSWORD.to_string(), password.to_string()),
    ];
    Ok((cmd, env))
}

/// Runs `git clone` on this machine and returns the destination and git's output.
///
/// `branch` is the branch to check out; empty follows the server's `HEAD`.
/// `password` is the session password, used only by password profiles.
/// Blocking: call it off the UI thread.
pub fn clone_repo(
    profile: &Profile,
    repo_path: &str,
    url: &str,
    parent: &Path,
    folder: &str,
    branch: &str,
    password: &str,
) -> Result<(PathBuf, String)> {
    let (dest, args) = clone_args(repo_path, url, parent, folder, branch)?;
    if !parent.is_dir() {
        return Err(anyhow!("{} is not a directory", parent.display()));
    }
    if dest.exists() {
        return Err(anyhow!("{} already exists", dest.display()));
    }
    let (ssh_command, extra_env) = transport(profile, password)?;

    let mut cmd = Command::new("git");
    cmd.args(&args)
        .env("GIT_SSH_COMMAND", ssh_command)
        // Our transport is not OpenSSH, so say how git should call it.
        .env("GIT_SSH_VARIANT", "ssh")
        // git must never stop for a credential prompt we cannot show.
        .env("GIT_TERMINAL_PROMPT", "0");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let out = cmd
        .output()
        .map_err(|e| anyhow!("could not start git (is it installed?): {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if out.status.success() {
        let note = checkout_after_clone(&dest);
        Ok((dest, format!("{text}{note}")))
    } else {
        Err(anyhow!(
            "git clone failed ({}): {}",
            out.status,
            text.trim()
        ))
    }
}

/// Runs a local git command inside `dir` and returns (succeeded, output).
fn git_local(dir: &Path, args: &[&str]) -> (bool, String) {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    match cmd.output() {
        Ok(out) => (
            out.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        ),
        Err(e) => (false, e.to_string()),
    }
}

/// Picks the branch to check out when the server's `HEAD` named one that does
/// not exist: the usual default names first, otherwise whatever came along.
pub fn preferred_branch(branches: &[String]) -> Option<String> {
    for wanted in ["main", "master", "trunk", "develop"] {
        if let Some(b) = branches.iter().find(|b| *b == wanted) {
            return Some(b.clone());
        }
    }
    branches.first().cloned()
}

/// Checks out a branch when the clone left no working tree.
///
/// A repository whose `HEAD` points at a branch that was never pushed (`main`
/// on the server, `master` in the history) clones every object but checks
/// nothing out, so the new directory looks empty apart from `.git`. Git only
/// warns about it, so do the checkout here and say what happened.
fn checkout_after_clone(dest: &Path) -> String {
    // A working tree is there when HEAD resolves to a commit.
    if git_local(dest, &["rev-parse", "--verify", "--quiet", "HEAD"]).0 {
        return String::new();
    }

    let (ok, refs) = git_local(
        dest,
        &[
            "for-each-ref",
            "--format=%(refname:lstrip=3)",
            "refs/remotes/origin",
        ],
    );
    let branches: Vec<String> = if ok {
        refs.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && l != "HEAD")
            .collect()
    } else {
        Vec::new()
    };

    let Some(branch) = preferred_branch(&branches) else {
        return "\nNothing was checked out: the repository has no commits yet.\n".to_string();
    };

    let (ok, out) = git_local(
        dest,
        &["checkout", "-B", &branch, "--track", &format!("origin/{branch}")],
    );
    if ok {
        format!(
            "\nThe server's HEAD names a branch that does not exist, so git checked nothing out; \
             checked out '{branch}' instead.\n"
        )
    } else {
        format!(
            "\nNothing was checked out and 'git checkout {branch}' failed: {}\n",
            out.trim()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_folder_defaults_to_the_repository_name() {
        assert_eq!(clone_dir_name("/volume1/git/proj.git"), "proj");
        assert_eq!(clone_dir_name("/volume1/git/proj.git/"), "proj");
        assert_eq!(clone_dir_name("/volume1/homes/me/proj/.git"), "proj");
    }

    #[test]
    fn clone_args_reject_a_path_as_folder_name() {
        let parent = Path::new("/tmp/dest");
        for bad in ["..", ".", "a/b", "a\\b"] {
            assert!(
                clone_args("/volume1/git/p.git", "ssh://x/p.git", parent, bad, "").is_err(),
                "accepted {bad:?}"
            );
        }
        let (dest, args) =
            clone_args("/volume1/git/p.git", "ssh://x/p.git", parent, "", "").unwrap();
        assert_eq!(dest, parent.join("p"));
        assert_eq!(args[0], "clone");
        assert_eq!(args[2], "ssh://x/p.git");
    }

    #[test]
    fn a_branch_is_passed_to_git_and_cannot_be_an_option() {
        let parent = Path::new("/tmp/dest");
        let (_, args) =
            clone_args("/volume1/git/p.git", "ssh://x/p.git", parent, "", "dev").unwrap();
        assert_eq!(args[2], "--branch");
        assert_eq!(args[3], "dev");
        assert!(
            clone_args("/volume1/git/p.git", "ssh://x/p.git", parent, "", "--upload-pack=evil")
                .is_err()
        );
    }


    /// Builds a bare repository whose HEAD names a branch nobody pushed, the
    /// shape that makes `git clone` check nothing out.
    #[test]
    fn a_clone_with_a_dangling_head_still_gets_a_working_tree() {
        let tmp = std::env::temp_dir().join(format!("ygit-clone-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let work = tmp.join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("hello.txt"), "hi").unwrap();
        for args in [
            vec!["init", "-q", "-b", "master"],
            vec!["config", "user.email", "t@example.invalid"],
            vec!["config", "user.name", "test"],
            vec!["add", "hello.txt"],
            vec!["commit", "-qm", "first"],
        ] {
            assert!(git_local(&work, &args).0, "git {args:?} failed");
        }

        let bare = tmp.join("origin.git");
        assert!(
            Command::new("git")
                .arg("clone")
                .arg("-q")
                .arg("--bare")
                .arg(&work)
                .arg(&bare)
                .status()
                .unwrap()
                .success()
        );
        // The server says main; the history only has master.
        assert!(git_local(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]).0);

        let dest = tmp.join("clone");
        assert!(
            Command::new("git")
                .arg("clone")
                .arg("-q")
                .arg(&bare)
                .arg(&dest)
                .status()
                .unwrap()
                .success()
        );
        // This is the state the user hit: a clone with no source files.
        assert!(!dest.join("hello.txt").exists());

        let note = checkout_after_clone(&dest);
        assert!(note.contains("checked out 'master'"), "note was {note:?}");
        assert!(dest.join("hello.txt").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_default_branch_is_preferred_over_whatever_comes_first() {
        let branches = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            preferred_branch(&branches(&["dev", "master", "main"])).as_deref(),
            Some("main")
        );
        assert_eq!(
            preferred_branch(&branches(&["dev", "master"])).as_deref(),
            Some("master")
        );
        assert_eq!(preferred_branch(&branches(&["topic"])).as_deref(), Some("topic"));
        assert_eq!(preferred_branch(&[]), None);
    }

    #[test]
    fn ssh_command_carries_port_key_and_host_policy() {
        let mut p = Profile {
            port: 2222,
            key_path: r"C:\keys\my key".to_string(),
            ..Default::default()
        };
        let cmd = git_ssh_command(&p);
        assert!(cmd.contains("-p 2222"));
        assert!(cmd.contains(r"'C:\keys\my key'"));
        assert!(cmd.contains("BatchMode=yes"));
        assert!(cmd.contains("StrictHostKeyChecking=accept-new"));

        p.strict_host_key = true;
        assert!(git_ssh_command(&p).contains("StrictHostKeyChecking=yes"));
    }
}
