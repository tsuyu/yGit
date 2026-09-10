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
| `src/terminal.rs` | Launches the system `ssh` client in a terminal window |
| `src/config.rs` | Profile persistence |

The SSH work runs on a dedicated thread with a current-thread tokio runtime; the UI talks to
it through channels, so the window never blocks on the network.
