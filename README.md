# yGit

Desktop GUI (Rust + egui) for browsing git repositories hosted on a Synology NAS over SSH.

## What it does

- Connects to the NAS over SSH with a private key or a password (pure-Rust `russh`, no OpenSSH libraries needed).
- Scans configured roots (default `/volume1/git`, `/volume1/homes`) and lists every git repository it finds — bare repos and working copies alike.
- Every row in the list carries its tip commit (short hash, relative date, subject); repositories
  without commits say so.
- Selecting a repository shows its last 50 commits — hash, relative date (hover for the ISO date),
  author, subject — plus branches, on-disk size, `HEAD`, and a one-click copy of the `ssh://` clone URL.
  Clicking a hash copies the whole commit line.
- Creates new bare repositories on the NAS (`git init --bare`, optionally `--shared=group`), with an
  optional description. Names are restricted to `[A-Za-z0-9._-]`, may not start with `.` or `-`, and
  may not contain `..`, so nothing can escape the chosen root; the `.git` suffix is added if missing.
- Clones a repository to a folder on this computer: pick the destination, git runs locally and its
  output lands in the log. Key profiles clone through the system `ssh` with the profile's port, key
  and host-key policy (`GIT_SSH_COMMAND`). Password profiles clone through yGit's own transport
  (`ygit --ssh-proxy`, see below), because `git clone` has no terminal on which `ssh` could ask for
  a password. When the server's `HEAD` names a branch that was never pushed (`main` on the server,
  `master` in the history) git clones the objects but checks nothing out, leaving what looks like an
  empty folder; the clone then checks out the real default branch itself and says so in the log.
  A branch can be picked in the dialog (the list comes from the server, and a name can also be typed);
  leaving it empty follows the server's `HEAD`.
- Deletes a repository on the server after a typed-name confirmation. The path must sit inside one
  of the configured roots and the server re-checks that it really is a git repository before `rm -rf`,
  so a stale listing or a mistyped root cannot delete anything else. For a working copy the default is
  to remove only its `.git`; a checkbox removes the whole project directory.
- Opens a real interactive SSH session in a terminal window — either at the NAS home directory or directly inside a repository — by launching the system `ssh` client.
- Runs ad-hoc remote commands from the bottom bar.

## Build and run

```sh
cargo run --release
```

## Configuration

Profiles are stored in `%APPDATA%\ygit\config.json` (Windows) or `~/.config/ygit/config.json`.
Passwords and key passphrases are never written to disk.

## Host key handling

By default an unknown host key is trusted on first use and appended to your `known_hosts`
file; the fingerprint is printed in the log. A key that **changed** is always rejected.
Tick "Strict host key check" to reject unknown hosts as well.

## Layout

| File | Purpose |
| --- | --- |
| `src/main.rs` | egui application, panels, event pump |
| `src/ssh.rs` | SSH worker thread, repository discovery and detail queries |
| `src/terminal.rs` | Launches the system `ssh` client, and runs local `git clone` |
| `src/proxy.rs` | `ygit --ssh-proxy`: the SSH transport git uses for password profiles |
| `src/config.rs` | Profile persistence |

The SSH work runs on a dedicated thread with a current-thread tokio runtime; the UI talks to
it through channels, so the window never blocks on the network.

## Cloning with a password profile

`ssh` can only ask for a password on a terminal, and `git clone` gives it none. So for password
profiles the app sets `GIT_SSH_COMMAND` to itself: git starts `ygit --ssh-proxy`, which opens the
session with `russh` and pipes git's stdin and stdout through the SSH channel.

The password comes from the connected session - it is kept in memory only, dropped on disconnect,
and handed to the git child through its environment, which on the supported platforms only this
user can read. It is still never written to disk. Connect before cloning; without a live session
there is no password to reuse and the Clone button stays disabled.
