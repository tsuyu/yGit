use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use russh::client::{self, Handle};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::ChannelMsg;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::config::{default_key_candidates, AuthKind, Profile};

/// The tip commit of a repository, as shown in the repository list.
#[derive(Debug, Clone)]
pub struct CommitBrief {
    pub hash: String,
    pub when: String,
    pub subject: String,
}

/// One entry of a repository's history.
#[derive(Debug, Clone)]
pub struct Commit {
    pub hash: String,
    pub author: String,
    pub date: String,
    pub when: String,
    pub subject: String,
}

/// A git repository discovered on the server.
#[derive(Debug, Clone)]
pub struct Repo {
    pub path: String,
    pub name: String,
    pub description: String,
    pub bare: bool,
    pub last: Option<CommitBrief>,
}

/// Outcome of a repository scan.
#[derive(Debug, Clone, Default)]
pub struct RepoList {
    pub repos: Vec<Repo>,
    /// Set when git could not be located on the server at all.
    pub no_git: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RepoDetail {
    pub commits: Vec<Commit>,
    pub branches: Vec<String>,
    pub size: String,
    pub head: String,
    pub truncated: bool,
    /// Why the log is empty, when git itself complained.
    pub error: String,
    /// `git --version` on the server, or the reason git could not be found.
    pub git: String,
}

/// How many commits a detail query fetches.
const LOG_LIMIT: usize = 50;

/// Locates git and defines the `rgit` helper used by the remote scripts.
///
/// DSM does not put every git on the PATH of a non-interactive shell, so the
/// well-known package locations are probed too. `safe.directory` is relaxed
/// because repositories on a NAS are normally owned by another user, which
/// modern git otherwise refuses to read ("detected dubious ownership").
const GIT_PREAMBLE: &str = concat!(
    "GIT=$(command -v git 2>/dev/null)\n",
    "if [ -z \"$GIT\" ]; then\n",
    "  for c in /usr/bin/git /usr/local/bin/git /opt/bin/git \\\n",
    "           /volume1/@appstore/Git/bin/git /var/packages/Git/target/bin/git; do\n",
    "    if [ -x \"$c\" ]; then GIT=$c; break; fi\n",
    "  done\n",
    "fi\n",
    "rgit() { d=$1; shift; \"$GIT\" -c safe.directory='*' --git-dir=\"$d\" \"$@\"; }\n",
);

/// Field separator used inside git --format strings (ASCII unit separator,
/// written as `%x1f`), so subjects containing tabs or pipes stay intact.
const SEP: char = '\x1f';

/// Requests sent from the UI thread to the SSH worker.
pub enum Cmd {
    Connect {
        profile: Profile,
        password: String,
        passphrase: String,
    },
    ListRepos {
        roots: Vec<String>,
    },
    Detail {
        path: String,
    },
    Exec {
        label: String,
        script: String,
    },
    CreateRepo {
        root: String,
        name: String,
        description: String,
        shared: bool,
    },
    DeleteRepo {
        /// The repository directory as reported by the scan.
        path: String,
        /// Roots the path must live under; the server refuses anything else.
        roots: Vec<String>,
        /// For a working copy, delete the whole project directory instead of
        /// just its `.git`.
        whole_tree: bool,
    },
    Disconnect,
}

/// Events sent from the SSH worker back to the UI thread.
pub enum Evt {
    Log(String),
    Connected { banner: String },
    Disconnected,
    Failed(String),
    Repos(RepoList),
    Detail(String, RepoDetail),
    Output { label: String, text: String },
    Created { path: String },
    Deleted { path: String },
    /// A local `git clone` finished; `dest` is the directory it wrote.
    Cloned { dest: String },
    /// A local `git clone` failed.
    CloneFailed(String),
    Busy(bool),
}

pub struct Client {
    host: String,
    port: u16,
    strict: bool,
    log: UnboundedSender<Evt>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            let _ = self
                .log
                .send(Evt::Log("host presented a certificate; rejected".into()));
            return Ok(false);
        };
        let fp = key.fingerprint(Default::default()).to_string();
        match russh::keys::check_known_hosts(&self.host, self.port, key) {
            Ok(true) => {
                let _ = self.log.send(Evt::Log(format!("host key known ({fp})")));
                Ok(true)
            }
            Ok(false) => {
                if self.strict {
                    let _ = self.log.send(Evt::Log(format!(
                        "host key {fp} not in known_hosts and strict mode is on; rejected"
                    )));
                    return Ok(false);
                }
                match russh::keys::known_hosts::learn_known_hosts(&self.host, self.port, key) {
                    Ok(()) => {
                        let _ = self.log.send(Evt::Log(format!(
                            "new host key {fp} trusted and written to known_hosts"
                        )));
                    }
                    Err(e) => {
                        let _ = self
                            .log
                            .send(Evt::Log(format!("could not record host key {fp}: {e}")));
                    }
                }
                Ok(true)
            }
            Err(e) => {
                let _ = self.log.send(Evt::Log(format!(
                    "HOST KEY MISMATCH for {}:{} ({fp}): {e} - refusing to connect",
                    self.host, self.port
                )));
                Ok(false)
            }
        }
    }
}

/// Spawns the SSH worker on its own thread with a current-thread tokio runtime.
pub fn spawn(evt: std::sync::mpsc::Sender<Evt>, ctx: egui::Context) -> UnboundedSender<Cmd> {
    let (cmd_tx, cmd_rx) = unbounded_channel();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = evt.send(Evt::Failed(format!("runtime: {e}")));
                return;
            }
        };
        rt.block_on(worker(cmd_rx, evt, ctx));
    });
    cmd_tx
}

struct Bridge {
    tx: std::sync::mpsc::Sender<Evt>,
    ctx: egui::Context,
}

impl Bridge {
    fn send(&self, e: Evt) {
        let _ = self.tx.send(e);
        self.ctx.request_repaint();
    }
}

async fn worker(
    mut rx: UnboundedReceiver<Cmd>,
    tx: std::sync::mpsc::Sender<Evt>,
    ctx: egui::Context,
) {
    let bridge = Bridge { tx, ctx };
    let mut session: Option<Handle<Client>> = None;

    while let Some(cmd) = rx.recv().await {
        bridge.send(Evt::Busy(true));
        match cmd {
            Cmd::Connect {
                profile,
                password,
                passphrase,
            } => match connect(&profile, &password, &passphrase, &log_forwarder(&bridge)).await {
                Ok(handle) => {
                    let probe = "uname -a; git --version 2>/dev/null || echo 'git: not found in PATH'";
                    let banner = run(&handle, probe)
                        .await
                        .unwrap_or_else(|e| format!("(probe failed: {e})"));
                    session = Some(handle);
                    bridge.send(Evt::Connected {
                        banner: banner.trim().to_string(),
                    });
                }
                Err(e) => {
                    session = None;
                    bridge.send(Evt::Failed(format!("{e:#}")));
                }
            },
            Cmd::ListRepos { roots } => match session.as_ref() {
                Some(h) => match list_repos(h, &roots).await {
                    Ok(list) => {
                        bridge.send(Evt::Log(format!("found {} repositories", list.repos.len())));
                        if list.no_git {
                            bridge.send(Evt::Log(
                                "git was not found on the server, so no commit information                                  could be read (install the Git Server package)"
                                    .into(),
                            ));
                        }
                        bridge.send(Evt::Repos(list));
                    }
                    Err(e) => bridge.send(Evt::Failed(format!("listing failed: {e:#}"))),
                },
                None => bridge.send(Evt::Failed("not connected".into())),
            },
            Cmd::Detail { path } => match session.as_ref() {
                Some(h) => match detail(h, &path).await {
                    Ok(d) => bridge.send(Evt::Detail(path, d)),
                    Err(e) => bridge.send(Evt::Failed(format!("detail failed: {e:#}"))),
                },
                None => bridge.send(Evt::Failed("not connected".into())),
            },
            Cmd::Exec { label, script } => match session.as_ref() {
                Some(h) => match run(h, &script).await {
                    Ok(text) => bridge.send(Evt::Output { label, text }),
                    Err(e) => bridge.send(Evt::Failed(format!("exec failed: {e:#}"))),
                },
                None => bridge.send(Evt::Failed("not connected".into())),
            },
            Cmd::CreateRepo {
                root,
                name,
                description,
                shared,
            } => match session.as_ref() {
                Some(h) => match create_repo(h, &root, &name, &description, shared).await {
                    Ok(path) => {
                        bridge.send(Evt::Log(format!("created {path}")));
                        bridge.send(Evt::Created { path });
                    }
                    Err(e) => bridge.send(Evt::Failed(format!("create failed: {e:#}"))),
                },
                None => bridge.send(Evt::Failed("not connected".into())),
            },
            Cmd::DeleteRepo {
                path,
                roots,
                whole_tree,
            } => match session.as_ref() {
                Some(h) => match delete_repo(h, &path, &roots, whole_tree).await {
                    Ok(removed) => {
                        bridge.send(Evt::Log(format!("deleted {removed}")));
                        bridge.send(Evt::Deleted { path: removed });
                    }
                    Err(e) => bridge.send(Evt::Failed(format!("delete failed: {e:#}"))),
                },
                None => bridge.send(Evt::Failed("not connected".into())),
            },
            Cmd::Disconnect => {
                if let Some(h) = session.take() {
                    let _ = h
                        .disconnect(russh::Disconnect::ByApplication, "bye", "en")
                        .await;
                }
                bridge.send(Evt::Disconnected);
            }
        }
        bridge.send(Evt::Busy(false));
    }
}

/// Opens an authenticated session. `log` receives the progress lines; the UI
/// feeds it through [`log_forwarder`], the git transport proxy prints them to
/// stderr.
pub async fn connect(
    profile: &Profile,
    password: &str,
    passphrase: &str,
    log: &UnboundedSender<Evt>,
) -> Result<Handle<Client>> {
    if profile.host.trim().is_empty() {
        return Err(anyhow!("host is empty"));
    }
    if profile.user.trim().is_empty() {
        return Err(anyhow!("user is empty"));
    }

    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(300)),
        keepalive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    });

    let handler = Client {
        host: profile.host.clone(),
        port: profile.port,
        strict: profile.strict_host_key,
        log: log.clone(),
    };

    let _ = log.send(Evt::Log(format!(
        "connecting to {}@{}:{}",
        profile.user, profile.host, profile.port
    )));

    let mut handle = client::connect(config, (profile.host.as_str(), profile.port), handler)
        .await
        .with_context(|| format!("tcp/ssh handshake to {}:{}", profile.host, profile.port))?;

    match profile.auth {
        AuthKind::Password => {
            if password.is_empty() {
                return Err(anyhow!("password auth selected but no password entered"));
            }
            let res = handle
                .authenticate_password(profile.user.clone(), password.to_string())
                .await?;
            if !res.success() {
                return Err(anyhow!("password rejected by server"));
            }
            let _ = log.send(Evt::Log("authenticated with password".into()));
        }
        AuthKind::Key => {
            let candidates: Vec<std::path::PathBuf> = if profile.key_path.trim().is_empty() {
                default_key_candidates()
            } else {
                vec![std::path::PathBuf::from(profile.key_path.trim())]
            };
            if candidates.is_empty() {
                return Err(anyhow!(
                    "no private key found; set one in the profile or use password auth"
                ));
            }
            let pass = (!passphrase.is_empty()).then_some(passphrase);
            let mut last_err = None;
            let mut ok = false;
            for key_path in &candidates {
                let key = match russh::keys::load_secret_key(key_path, pass) {
                    Ok(k) => k,
                    Err(e) => {
                        last_err = Some(anyhow!("{}: {e}", key_path.display()));
                        continue;
                    }
                };
                let hash = handle.best_supported_rsa_hash().await?.flatten();
                let res = handle
                    .authenticate_publickey(
                        profile.user.clone(),
                        PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                    )
                    .await?;
                if res.success() {
                    let _ = log.send(Evt::Log(format!(
                        "authenticated with {}",
                        key_path.display()
                    )));
                    ok = true;
                    break;
                }
                last_err = Some(anyhow!("{}: key rejected", key_path.display()));
            }
            if !ok {
                return Err(last_err.unwrap_or_else(|| anyhow!("public key auth failed")));
            }
        }
    }

    Ok(handle)
}

/// The connection handler runs outside the worker loop, so give it a plain
/// sender that forwards into the UI channel.
fn log_forwarder(bridge: &Bridge) -> UnboundedSender<Evt> {
    let (tx, mut rx) = unbounded_channel::<Evt>();
    let out = bridge.tx.clone();
    let ctx = bridge.ctx.clone();
    tokio::spawn(async move {
        while let Some(e) = rx.recv().await {
            let _ = out.send(e);
            ctx.request_repaint();
        }
    });
    tx
}

/// Runs a command on the server and returns stdout+stderr.
async fn run(handle: &Handle<Client>, script: &str) -> Result<String> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, script).await?;

    let mut out = Vec::new();
    let mut code: Option<u32> = None;
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { ref data } => out.extend_from_slice(data),
            ChannelMsg::ExtendedData { ref data, .. } => out.extend_from_slice(data),
            ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
            ChannelMsg::Eof | ChannelMsg::Close => break,
            _ => {}
        }
    }
    let _ = channel.close().await;

    let text = String::from_utf8_lossy(&out).to_string();
    match code {
        Some(0) | None => Ok(text),
        Some(c) => Err(anyhow!("remote exit {c}: {}", text.trim())),
    }
}

/// Quotes a value for POSIX `sh` by wrapping it in single quotes.
fn shq(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Builds the script that walks the roots and prints one TSV line per repository.
fn list_script(roots: &[String]) -> Result<String> {
    let quoted: Vec<String> = roots
        .iter()
        .map(|r| r.trim())
        .filter(|r| !r.is_empty())
        .map(shq)
        .collect();
    if quoted.is_empty() {
        return Err(anyhow!("no repository roots configured"));
    }

    // A directory is a git repo when it holds HEAD plus objects/ and refs/.
    // That matches bare repos (/volume1/git/foo.git) and the .git directory
    // of a working copy alike.
    Ok(format!(
        concat!(
            "{}",
            "for r in {}; do\n",
            "  [ -d \"$r\" ] || continue\n",
            "  find \"$r\" -maxdepth 5 -name HEAD -type f 2>/dev/null\n",
            "done | while IFS= read -r h; do\n",
            "  d=$(dirname \"$h\")\n",
            "  [ -d \"$d/objects\" ] && [ -d \"$d/refs\" ] || continue\n",
            "  desc=''\n",
            "  if [ -f \"$d/description\" ]; then\n",
            "    desc=$(head -n 1 \"$d/description\" 2>/dev/null)\n",
            "    case \"$desc\" in \"Unnamed repository\"*) desc='' ;; esac\n",
            "  fi\n",
            "  bare=1\n",
            "  case \"$d\" in */.git) bare=0 ;; esac\n",
            "  last=''\n",
            "  if [ -n \"$GIT\" ]; then\n",
            "    fmt='%h%x1f%cr%x1f%s'\n",
            // HEAD can name a branch that does not exist yet (pushed as master
            // while HEAD says main), so fall back to the newest of all refs.
            "    last=$(rgit \"$d\" log -1 --format=\"$fmt\" 2>/dev/null | head -n 1)\n",
            "    if [ -z \"$last\" ]; then\n",
            "      last=$(rgit \"$d\" log -1 --all --format=\"$fmt\" 2>/dev/null | head -n 1)\n",
            "    fi\n",
            "  fi\n",
            "  printf '%s\\t%s\\t%s\\t%s\\n' \"$d\" \"$bare\" \"$desc\" \"$last\"\n",
            "done\n",
            "[ -n \"$GIT\" ] || echo '<<<NOGIT'"
        ),
        GIT_PREAMBLE,
        quoted.join(" ")
    ))
}

async fn list_repos(handle: &Handle<Client>, roots: &[String]) -> Result<RepoList> {
    let script = list_script(roots)?;
    let out = run(handle, &script).await?;
    let mut list = parse_repo_lines(&out);
    list.repos.sort_by_key(|r| r.name.to_lowercase());
    Ok(list)
}

/// Parses the TSV emitted by [`list_script`].
fn parse_repo_lines(out: &str) -> RepoList {
    let no_git = out.lines().any(|l| l.trim() == "<<<NOGIT");
    let repos = out
        .lines()
        .filter(|l| !l.trim_start().starts_with("<<<"))
        .filter_map(|line| {
            let mut parts = line.splitn(4, '\t');
            let path = parts.next()?.trim().to_string();
            if path.is_empty() {
                return None;
            }
            let bare = parts.next().unwrap_or("1") == "1";
            let description = parts.next().unwrap_or("").trim().to_string();
            let last = parse_brief(parts.next().unwrap_or(""));
            let display = if bare {
                path.as_str()
            } else {
                path.strip_suffix("/.git").unwrap_or(path.as_str())
            };
            let name = display
                .rsplit('/')
                .next()
                .unwrap_or(display)
                .trim_end_matches(".git")
                .to_string();
            Some(Repo {
                path,
                name,
                description,
                bare,
                last,
            })
        })
        .collect();
    RepoList { repos, no_git }
}

/// Parses `hash<SEP>relative-date<SEP>subject` as produced by the list script.
fn parse_brief(field: &str) -> Option<CommitBrief> {
    let field = field.trim();
    if field.is_empty() {
        return None;
    }
    let mut it = field.split(SEP);
    let hash = it.next()?.trim().to_string();
    if hash.is_empty() {
        return None;
    }
    Some(CommitBrief {
        when: it.next().unwrap_or("").trim().to_string(),
        subject: it.next().unwrap_or("").trim().to_string(),
        hash,
    })
}

/// Parses `hash<SEP>author<SEP>iso-date<SEP>relative-date<SEP>subject`.
fn parse_commit(line: &str) -> Option<Commit> {
    let mut it = line.split(SEP);
    let hash = it.next()?.trim().to_string();
    if hash.is_empty() {
        return None;
    }
    Some(Commit {
        author: it.next().unwrap_or("").trim().to_string(),
        date: it.next().unwrap_or("").trim().to_string(),
        when: it.next().unwrap_or("").trim().to_string(),
        subject: it.next().unwrap_or("").trim().to_string(),
        hash,
    })
}

/// Builds the script that reports a repository's log, branches, HEAD and size.
/// One extra commit is requested so we can tell whether the history was cut off.
fn detail_script(path: &str) -> String {
    format!(
        concat!(
            "{}",
            "d={}\n",
            "echo '<<<LOG'\n",
            "fmt='%h%x1f%an%x1f%cI%x1f%cr%x1f%s'\n",
            // Errors are kept: an empty log is usually caused by git refusing to
            // read the repository, and the reason belongs in the UI.
            "log=$(rgit \"$d\" log -n {} --format=\"$fmt\" 2>&1)\n",
            "ok=$?\n",
            "if [ $ok -ne 0 ] || [ -z \"$log\" ]; then\n",
            "  alt=$(rgit \"$d\" log -n {} --all --format=\"$fmt\" 2>&1)\n",
            // The fallback only wins when it worked, so that a genuine error
            // message survives instead of being blanked out.
            "  if [ $? -eq 0 ] && [ -n \"$alt\" ]; then log=$alt; fi\n",
            "fi\n",
            "printf '%s\\n' \"$log\"\n",
            "echo '<<<BRANCHES'\n",
            "rgit \"$d\" for-each-ref --sort=-committerdate",
            " --format='%(refname:short)' refs/heads 2>/dev/null\n",
            "echo '<<<HEAD'\n",
            "cat \"$d/HEAD\" 2>/dev/null\n",
            "echo '<<<SIZE'\n",
            "du -sh \"$d\" 2>/dev/null | cut -f1\n",
            "echo '<<<GIT'\n",
            "if [ -n \"$GIT\" ]; then \"$GIT\" --version 2>&1 | head -n 1; else",
            " echo 'git was not found on the server (install the Git Server package)'; fi"
        ),
        GIT_PREAMBLE,
        shq(path),
        LOG_LIMIT + 1,
        LOG_LIMIT + 1
    )
}

async fn detail(handle: &Handle<Client>, path: &str) -> Result<RepoDetail> {
    let out = run(handle, &detail_script(path)).await?;
    Ok(parse_detail(&out))
}

/// Parses the sectioned output of [`detail_script`].
fn parse_detail(out: &str) -> RepoDetail {
    let mut d = RepoDetail::default();
    let mut section = "";
    for line in out.lines() {
        if let Some(name) = line.trim().strip_prefix("<<<") {
            section = match name {
                "LOG" => "log",
                "GIT" => "git",
                "BRANCHES" => "branches",
                "HEAD" => "head",
                "SIZE" => "size",
                _ => "",
            };
            continue;
        }
        match section {
            "log" => {
                if line.contains(SEP) {
                    if let Some(c) = parse_commit(line) {
                        d.commits.push(c);
                    }
                } else if !line.trim().is_empty() {
                    // No separator means git printed a diagnostic, not a commit.
                    if !d.error.is_empty() {
                        d.error.push('\n');
                    }
                    d.error.push_str(line.trim());
                }
            }
            "git" if !line.trim().is_empty() => {
                if d.git.is_empty() {
                    d.git = line.trim().to_string();
                }
            }
            "branches" if !line.trim().is_empty() => d.branches.push(line.trim().to_string()),
            "head" if !line.trim().is_empty() => {
                d.head = line.trim().trim_start_matches("ref: refs/heads/").to_string();
            }
            "size" if !line.trim().is_empty() => d.size = line.trim().to_string(),
            _ => {}
        }
    }
    if d.commits.len() > LOG_LIMIT {
        d.commits.truncate(LOG_LIMIT);
        d.truncated = true;
    }
    d
}

/// Rejects repository names that would escape the root or confuse the shell.
pub fn validate_repo_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(anyhow!("name is empty"));
    }
    if name.starts_with('.') || name.starts_with('-') {
        return Err(anyhow!("name may not start with '.' or '-'"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(anyhow!(
            "name may only contain letters, digits, '_', '-' and '.'"
        ));
    }
    if name.contains("..") {
        return Err(anyhow!("name may not contain '..'"));
    }
    let bare = if name.ends_with(".git") {
        name.to_string()
    } else {
        format!("{name}.git")
    };
    Ok(bare)
}

/// Builds the shell script that creates a bare repository, plus its full path.
fn create_script(
    root: &str,
    name: &str,
    description: &str,
    shared: bool,
) -> Result<(String, String)> {
    let root = root.trim().trim_end_matches('/');
    if root.is_empty() || !root.starts_with('/') {
        return Err(anyhow!("root must be an absolute path"));
    }
    let dir = validate_repo_name(name)?;
    let path = format!("{root}/{dir}");

    let shared_flag = if shared { "--shared=group" } else { "" };
    let script = format!(
        concat!(
            "d={}
",
            "command -v git >/dev/null 2>&1 || {{ echo 'git is not installed on the server ",
            "(install the Git Server package)'; exit 4; }}
",
            "[ -d {} ] || {{ echo 'root directory does not exist'; exit 5; }}
",
            "if [ -e \"$d\" ]; then echo 'a file or repository with that name already exists'; exit 6; fi
",
            "mkdir \"$d\" || {{ echo 'could not create the directory (permissions?)'; exit 7; }}
",
            "git init --bare {} \"$d\" >/dev/null || {{ rmdir \"$d\" 2>/dev/null; ",
            "echo 'git init --bare failed'; exit 8; }}
",
            "printf '%s\n' {} > \"$d/description\"
",
            "echo \"$d\""
        ),
        shq(&path),
        shq(root),
        shared_flag,
        shq(description.trim())
    );
    Ok((path, script))
}

/// Creates a bare repository under `root` and returns its full path.
async fn create_repo(
    handle: &Handle<Client>,
    root: &str,
    name: &str,
    description: &str,
    shared: bool,
) -> Result<String> {
    let (path, script) = create_script(root, name, description, shared)?;
    let out = run(handle, &script).await?;
    let reported = out.trim();
    Ok(if reported.is_empty() {
        path
    } else {
        reported.to_string()
    })
}

/// Works out what a delete actually removes, and checks the target is a
/// repository directory that lives under one of the configured roots.
///
/// `path` is the directory the scan reported: a bare repository, or the `.git`
/// of a working copy. With `whole_tree` the working copy's project directory is
/// removed instead of only its `.git`.
pub fn delete_target(path: &str, roots: &[String], whole_tree: bool) -> Result<String> {
    let path = path.trim().trim_end_matches('/');
    if path.is_empty() || !path.starts_with('/') {
        return Err(anyhow!("repository path must be absolute"));
    }
    if path.split('/').any(|c| c == ".." || c == ".") {
        return Err(anyhow!("repository path may not contain '.' or '..'"));
    }

    let target = match path.strip_suffix("/.git") {
        Some(tree) if whole_tree => tree,
        _ if whole_tree => return Err(anyhow!("only a working copy has a working tree")),
        _ => path,
    };
    if target.is_empty() || target == "/" {
        return Err(anyhow!("refusing to delete the filesystem root"));
    }

    // The target has to sit strictly inside a configured root, so a typo in the
    // path can never turn into a delete somewhere else on the NAS.
    let inside = roots.iter().any(|r| {
        let r = r.trim().trim_end_matches('/');
        !r.is_empty() && r.starts_with('/') && target.starts_with(&format!("{r}/"))
    });
    if !inside {
        return Err(anyhow!(
            "{target} is not inside any configured repository root"
        ));
    }
    Ok(target.to_string())
}

/// Builds the shell script that removes `target`, plus the path it removes.
///
/// The script re-checks on the server that the directory is really a git
/// repository (or, for a whole working copy, holds one), so a stale listing
/// cannot make it delete an unrelated directory.
fn delete_script(path: &str, roots: &[String], whole_tree: bool) -> Result<(String, String)> {
    let target = delete_target(path, roots, whole_tree)?;
    let check = if whole_tree {
        "[ -d \"$d/.git\" ] || { echo 'not a working copy (no .git directory); refusing to delete'; exit 6; }\n"
    } else {
        concat!(
            "{ [ -f \"$d/HEAD\" ] && [ -d \"$d/objects\" ] && [ -d \"$d/refs\" ]; } || ",
            "{ echo 'not a git repository; refusing to delete'; exit 6; }\n"
        )
    };
    let script = format!(
        concat!(
            "d={}\n",
            "[ -d \"$d\" ] || {{ echo 'the directory no longer exists'; exit 5; }}\n",
            "{}",
            "rm -rf -- \"$d\" || {{ echo 'could not delete (permissions?)'; exit 7; }}\n",
            "[ -e \"$d\" ] && {{ echo 'the directory is still there after rm'; exit 8; }}\n",
            "echo \"$d\"\n"
        ),
        shq(&target),
        check
    );
    Ok((target, script))
}

/// Deletes a repository on the server and returns the path that was removed.
async fn delete_repo(
    handle: &Handle<Client>,
    path: &str,
    roots: &[String],
    whole_tree: bool,
) -> Result<String> {
    let (target, script) = delete_script(path, roots, whole_tree)?;
    let out = run(handle, &script).await?;
    let reported = out.trim();
    Ok(if reported.is_empty() {
        target
    } else {
        reported.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_normalised_and_checked() {
        assert_eq!(validate_repo_name("foo").unwrap(), "foo.git");
        assert_eq!(validate_repo_name(" bar.git ").unwrap(), "bar.git");
        for bad in ["", "../evil", ".hidden", "-rf", "a/b", "x;rm -rf /", "a..b"] {
            assert!(validate_repo_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn script_quotes_hostile_input() {
        let (path, script) =
            create_script("/volume1/git", "proj", "it's a 'test'; rm -rf /", true).unwrap();
        assert_eq!(path, "/volume1/git/proj.git");
        assert!(script.contains("'/volume1/git/proj.git'"));
        assert!(script.contains("--shared=group"));
        // Single quotes in the description must be escaped, not closed, so the
        // shell never sees `; rm -rf /` outside quotes.
        assert!(script.contains(r"'it'\''s a '\''test'\''; rm -rf /'"));
    }

    #[test]
    fn list_output_is_parsed() {
        let sep = SEP;
        let out = format!(
            "/volume1/git/a.git	1	the a repo	ab12cd{sep}2 days ago{sep}fix: tabs	and pipes|
             /volume1/homes/me/proj/.git	0		
             
"
        );
        let repos = parse_repo_lines(&out).repos;
        assert_eq!(repos.len(), 2);

        assert_eq!(repos[0].name, "a");
        assert!(repos[0].bare);
        assert_eq!(repos[0].description, "the a repo");
        let last = repos[0].last.as_ref().unwrap();
        assert_eq!(last.hash, "ab12cd");
        assert_eq!(last.when, "2 days ago");
        // The subject keeps everything after the separator, tabs included.
        assert_eq!(last.subject, "fix: tabs	and pipes|");

        assert_eq!(repos[1].name, "proj");
        assert!(!repos[1].bare);
        assert!(repos[1].last.is_none());
    }

    #[test]
    fn missing_git_is_reported() {
        let list = parse_repo_lines("/volume1/git/a.git	1		
<<<NOGIT
");
        assert!(list.no_git);
        assert_eq!(list.repos.len(), 1);
        assert!(list.repos[0].last.is_none());
    }

    #[test]
    fn git_diagnostics_become_an_error_not_a_commit() {
        let out = concat!(
            "<<<LOG\n",
            "fatal: detected dubious ownership in repository at '/volume1/git/a.git'\n",
            "<<<BRANCHES\nmain\n",
            "<<<HEAD\nref: refs/heads/main\n",
            "<<<SIZE\n1.0M\n",
            "<<<GIT\ngit version 2.39.5\n"
        );
        let d = parse_detail(out);
        assert!(d.commits.is_empty());
        assert!(d.error.contains("dubious ownership"));
        assert_eq!(d.git, "git version 2.39.5");
        assert_eq!(d.branches, vec!["main"]);
    }

    #[test]
    fn detail_output_is_parsed_and_capped() {
        let sep = SEP;
        let mut out = String::from("<<<LOG
");
        for i in 0..(LOG_LIMIT + 1) {
            out.push_str(&format!(
                "h{i}{sep}Ada{sep}2026-01-0{d}T00:00:00+00:00{sep}{i} days ago{sep}commit {i}
",
                d = i % 9 + 1
            ));
        }
        out.push_str("<<<BRANCHES
main
dev
<<<HEAD
ref: refs/heads/main
<<<SIZE
4.0K
");

        let d = parse_detail(&out);
        assert_eq!(d.commits.len(), LOG_LIMIT);
        assert!(d.truncated);
        assert_eq!(d.commits[0].hash, "h0");
        assert_eq!(d.commits[0].author, "Ada");
        assert_eq!(d.commits[0].subject, "commit 0");
        assert_eq!(d.branches, vec!["main", "dev"]);
        assert_eq!(d.head, "main");
        assert_eq!(d.size, "4.0K");
    }

    #[test]
    fn empty_roots_are_rejected() {
        assert!(list_script(&[]).is_err());
        assert!(list_script(&["   ".to_string()]).is_err());
    }

    #[test]
    fn root_must_be_absolute() {
        assert!(create_script("relative/path", "x", "", false).is_err());
    }

    #[test]
    fn delete_only_touches_repositories_under_a_root() {
        let roots = vec!["/volume1/git".to_string(), "/volume1/homes/".to_string()];

        assert_eq!(
            delete_target("/volume1/git/a.git", &roots, false).unwrap(),
            "/volume1/git/a.git"
        );
        // A working copy deletes its .git by default, the project dir on demand.
        assert_eq!(
            delete_target("/volume1/homes/me/proj/.git", &roots, false).unwrap(),
            "/volume1/homes/me/proj/.git"
        );
        assert_eq!(
            delete_target("/volume1/homes/me/proj/.git", &roots, true).unwrap(),
            "/volume1/homes/me/proj"
        );

        for bad in [
            "",
            "relative/a.git",
            "/",
            "/volume1/git",           // the root itself
            "/volume1/gitolite/a.git", // shares a prefix but is not inside
            "/etc",
            "/volume1/git/../../etc",
        ] {
            assert!(delete_target(bad, &roots, false).is_err(), "accepted {bad:?}");
        }
        // A bare repository has no working tree to delete.
        assert!(delete_target("/volume1/git/a.git", &roots, true).is_err());
    }

    #[test]
    fn delete_script_quotes_and_verifies() {
        let roots = vec!["/volume1/git".to_string()];
        let (target, script) = delete_script("/volume1/git/it's.git", &roots, false).unwrap();
        assert_eq!(target, "/volume1/git/it's.git");
        assert!(script.contains(r"d='/volume1/git/it'\''s.git'"));
        assert!(script.contains("not a git repository"));
        assert!(script.contains("rm -rf -- \"$d\""));

        let (target, script) =
            delete_script("/volume1/git/proj/.git", &roots, true).unwrap();
        assert_eq!(target, "/volume1/git/proj");
        assert!(script.contains("not a working copy"));
    }
}

#[cfg(test)]
mod dump {
    #[test]
    #[ignore = "dev helper: writes the generated scripts for shell testing"]
    fn scripts() {
        let dir = std::env::var("YGIT_DUMP").unwrap();
        let root = std::env::var("YGIT_ROOT").unwrap();
        std::fs::write(format!("{dir}/list.sh"), super::list_script(&[root.clone()]).unwrap()).unwrap();
        std::fs::write(
            format!("{dir}/detail.sh"),
            super::detail_script(&std::env::var("YGIT_REPO").unwrap()),
        )
        .unwrap();
    }
}
