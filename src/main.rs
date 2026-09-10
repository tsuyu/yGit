#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod ssh;
mod terminal;

use std::sync::mpsc::{channel, Receiver};

use config::{AuthKind, Config, Profile};
use eframe::egui;
use ssh::{Cmd, Evt, Repo, RepoDetail};
use tokio::sync::mpsc::UnboundedSender;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1080.0, 700.0])
            .with_min_inner_size([760.0, 480.0])
            .with_title("yGit - Synology git browser"),
        ..Default::default()
    };
    eframe::run_native(
        "yGit",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc.egui_ctx.clone())))),
    )
}

/// State of the "create repository" dialog.
struct NewRepo {
    root: String,
    name: String,
    description: String,
    shared: bool,
    error: String,
}

#[derive(PartialEq, Eq)]
enum Conn {
    Idle,
    Connecting,
    Up,
}

struct App {
    cfg: Config,
    password: String,
    passphrase: String,
    cmd_tx: UnboundedSender<Cmd>,
    evt_rx: Receiver<Evt>,
    conn: Conn,
    busy: bool,
    banner: String,
    log: Vec<String>,
    repos: Vec<Repo>,
    filter: String,
    selected: Option<usize>,
    detail: Option<(String, RepoDetail)>,
    output: Option<(String, String)>,
    roots_edit: String,
    exec_input: String,
    new_repo: Option<NewRepo>,
    no_git: bool,
    status: String,
}

impl App {
    fn new(ctx: egui::Context) -> Self {
        let cfg = Config::load();
        let roots_edit = cfg.current().repo_roots.join("\n");
        let (evt_tx, evt_rx) = channel();
        let cmd_tx = ssh::spawn(evt_tx, ctx);
        Self {
            cfg,
            password: String::new(),
            passphrase: String::new(),
            cmd_tx,
            evt_rx,
            conn: Conn::Idle,
            busy: false,
            banner: String::new(),
            log: Vec::new(),
            repos: Vec::new(),
            filter: String::new(),
            selected: None,
            detail: None,
            output: None,
            roots_edit,
            exec_input: String::new(),
            new_repo: None,
            no_git: false,
            status: "not connected".to_string(),
        }
    }

    fn push_log(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
        if self.log.len() > 500 {
            self.log.drain(..self.log.len() - 500);
        }
    }

    fn drain_events(&mut self) {
        while let Ok(evt) = self.evt_rx.try_recv() {
            match evt {
                Evt::Log(l) => self.push_log(l),
                Evt::Busy(b) => self.busy = b,
                Evt::Connected { banner } => {
                    self.conn = Conn::Up;
                    self.banner = banner;
                    self.status = "connected".to_string();
                    self.push_log("connected");
                    self.password.clear();
                    self.passphrase.clear();
                    self.refresh_repos();
                }
                Evt::Disconnected => {
                    self.conn = Conn::Idle;
                    self.banner.clear();
                    self.repos.clear();
                    self.selected = None;
                    self.detail = None;
                    self.status = "disconnected".to_string();
                    self.push_log("disconnected");
                }
                Evt::Failed(e) => {
                    if self.conn == Conn::Connecting {
                        self.conn = Conn::Idle;
                    }
                    self.status = format!("error: {e}");
                    self.push_log(format!("ERROR {e}"));
                }
                Evt::Repos(list) => {
                    self.status = if list.no_git {
                        format!("{} repositories (git missing on server)", list.repos.len())
                    } else {
                        format!("{} repositories", list.repos.len())
                    };
                    self.no_git = list.no_git;
                    self.repos = list.repos;
                    self.selected = None;
                    self.detail = None;
                }
                Evt::Created { path } => {
                    self.new_repo = None;
                    self.status = format!("created {path}");
                    self.refresh_repos();
                }
                Evt::Detail(path, d) => self.detail = Some((path, d)),
                Evt::Output { label, text } => {
                    self.push_log(format!("{label} done"));
                    self.output = Some((label, text));
                }
            }
        }
    }

    fn sync_roots(&mut self) {
        let roots: Vec<String> = self
            .roots_edit
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        self.cfg.current_mut().repo_roots = roots;
    }

    fn connect(&mut self) {
        self.sync_roots();
        if let Err(e) = self.cfg.save() {
            self.push_log(format!("could not save config: {e}"));
        }
        self.conn = Conn::Connecting;
        self.status = "connecting...".to_string();
        let _ = self.cmd_tx.send(Cmd::Connect {
            profile: self.cfg.current().clone(),
            password: self.password.clone(),
            passphrase: self.passphrase.clone(),
        });
    }

    fn refresh_repos(&mut self) {
        self.sync_roots();
        self.status = "scanning...".to_string();
        let _ = self.cmd_tx.send(Cmd::ListRepos {
            roots: self.cfg.current().repo_roots.clone(),
        });
    }

    fn visible_repos(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        self.repos
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                f.is_empty()
                    || r.name.to_lowercase().contains(&f)
                    || r.path.to_lowercase().contains(&f)
                    || r.description.to_lowercase().contains(&f)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn open_terminal(&mut self, cd: Option<String>) {
        let profile = self.cfg.current().clone();
        match terminal::open_ssh(&profile, cd.as_deref()) {
            Ok(cmd) => self.push_log(format!("launched: {cmd}")),
            Err(e) => {
                self.status = format!("terminal: {e}");
                self.push_log(format!("ERROR {e}"));
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("yGit");
                ui.separator();
                let (dot, text) = match self.conn {
                    Conn::Up => (egui::Color32::from_rgb(80, 200, 120), "online"),
                    Conn::Connecting => (egui::Color32::from_rgb(230, 190, 80), "connecting"),
                    Conn::Idle => (egui::Color32::from_rgb(200, 90, 90), "offline"),
                };
                ui.colored_label(dot, "\u{25CF}");
                ui.label(text);
                ui.separator();
                ui.label(&self.status);
                if self.busy {
                    ui.spinner();
                }
            });
        });

        egui::SidePanel::left("conn")
            .default_width(320.0)
            .show(ctx, |ui| self.connection_panel(ui));

        egui::TopBottomPanel::bottom("log")
            .resizable(true)
            .default_height(150.0)
            .show(ctx, |ui| self.log_panel(ui));

        egui::CentralPanel::default().show(ctx, |ui| self.repo_panel(ui));

        self.new_repo_window(ctx);
    }
}

impl App {
    fn connection_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.heading("Server");

        ui.horizontal(|ui| {
            ui.label("Profile");
            let names: Vec<String> = self.cfg.profiles.iter().map(|p| p.name.clone()).collect();
            let mut sel = self.cfg.selected;
            egui::ComboBox::from_id_salt("profile")
                .selected_text(names.get(sel).cloned().unwrap_or_default())
                .show_ui(ui, |ui| {
                    for (i, n) in names.iter().enumerate() {
                        ui.selectable_value(&mut sel, i, n);
                    }
                });
            if sel != self.cfg.selected {
                self.cfg.selected = sel;
                self.roots_edit = self.cfg.current().repo_roots.join("\n");
            }
            if ui.button("+").on_hover_text("new profile").clicked() {
                let mut p = Profile::default();
                p.name = format!("profile {}", self.cfg.profiles.len() + 1);
                self.cfg.profiles.push(p);
                self.cfg.selected = self.cfg.profiles.len() - 1;
                self.roots_edit = self.cfg.current().repo_roots.join("\n");
            }
            let removable = self.cfg.profiles.len() > 1;
            if ui
                .add_enabled(removable, egui::Button::new("-"))
                .on_hover_text("delete profile")
                .clicked()
            {
                let idx = self.cfg.selected;
                self.cfg.profiles.remove(idx);
                self.cfg.selected = 0;
                self.roots_edit = self.cfg.current().repo_roots.join("\n");
            }
        });

        let editable = self.conn == Conn::Idle;
        let mut auth = self.cfg.current().auth;

        ui.add_enabled_ui(editable, |ui| {
            let p = self.cfg.current_mut();
            egui::Grid::new("form")
                .num_columns(2)
                .spacing([8.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Name");
                    ui.text_edit_singleline(&mut p.name);
                    ui.end_row();

                    ui.label("Host");
                    ui.add(
                        egui::TextEdit::singleline(&mut p.host)
                            .hint_text("synology.local or 192.168.1.10"),
                    );
                    ui.end_row();

                    ui.label("Port");
                    ui.add(egui::DragValue::new(&mut p.port).range(1..=65535));
                    ui.end_row();

                    ui.label("User");
                    ui.text_edit_singleline(&mut p.user);
                    ui.end_row();
                });

            ui.horizontal(|ui| {
                ui.label("Auth");
                ui.radio_value(&mut auth, AuthKind::Key, "key");
                ui.radio_value(&mut auth, AuthKind::Password, "password");
            });
            p.auth = auth;
        });

        ui.add_enabled_ui(editable, |ui| match auth {
            AuthKind::Key => {
                ui.horizontal(|ui| {
                    let p = self.cfg.current_mut();
                    ui.add(
                        egui::TextEdit::singleline(&mut p.key_path)
                            .hint_text("~/.ssh/id_ed25519 (auto)")
                            .desired_width(190.0),
                    );
                    if ui.button("Browse").clicked() {
                        let start = dirs::home_dir().map(|h| h.join(".ssh"));
                        let mut dlg = rfd::FileDialog::new();
                        if let Some(s) = start {
                            dlg = dlg.set_directory(s);
                        }
                        if let Some(path) = dlg.pick_file() {
                            p.key_path = path.display().to_string();
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Passphrase");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.passphrase)
                            .password(true)
                            .hint_text("only if the key is encrypted"),
                    );
                });
            }
            AuthKind::Password => {
                ui.horizontal(|ui| {
                    ui.label("Password");
                    ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
                });
                ui.small("Passwords are never written to the config file.");
            }
        });

        ui.add_space(4.0);
        {
            let p = self.cfg.current_mut();
            ui.checkbox(
                &mut p.strict_host_key,
                "Strict host key check (reject unknown hosts)",
            );
        }
        if !self.cfg.current().strict_host_key {
            ui.small("Unknown host keys are trusted on first use and saved to known_hosts.");
        }

        ui.add_space(8.0);
        ui.label("Repository roots (one per line)");
        ui.add(
            egui::TextEdit::multiline(&mut self.roots_edit)
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            match self.conn {
                Conn::Up => {
                    if ui.button("Disconnect").clicked() {
                        let _ = self.cmd_tx.send(Cmd::Disconnect);
                    }
                    if ui.button("Rescan").clicked() {
                        self.refresh_repos();
                    }
                }
                Conn::Connecting => {
                    ui.add_enabled(false, egui::Button::new("Connecting..."));
                }
                Conn::Idle => {
                    if ui.button("Connect").clicked() {
                        self.connect();
                    }
                }
            }
            if ui.button("Save profile").clicked() {
                self.sync_roots();
                match self.cfg.save() {
                    Ok(()) => self.push_log(format!("saved {}", Config::path().display())),
                    Err(e) => self.push_log(format!("save failed: {e}")),
                }
            }
        });

        ui.add_space(6.0);
        if ui.button("Open SSH terminal").clicked() {
            self.open_terminal(None);
        }

        if !self.banner.is_empty() {
            ui.add_space(8.0);
            ui.separator();
            ui.small(&self.banner);
        }
    }

    fn repo_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Filter");
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("name, path or description")
                    .desired_width(240.0),
            );
            if ui.button("Clear").clicked() {
                self.filter.clear();
            }
            let visible = self.visible_repos().len();
            ui.label(format!("{visible}/{} shown", self.repos.len()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let connected = self.conn == Conn::Up;
                if ui
                    .add_enabled(connected, egui::Button::new("New repository"))
                    .clicked()
                {
                    self.new_repo = Some(NewRepo {
                        root: self
                            .cfg
                            .current()
                            .repo_roots
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "/volume1/git".to_string()),
                        name: String::new(),
                        description: String::new(),
                        shared: true,
                        error: String::new(),
                    });
                }
            });
        });
        ui.separator();

        if self.repos.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(match self.conn {
                    Conn::Up => "No repositories found. Check the repository roots on the left.",
                    _ => "Connect to a server to list repositories.",
                });
            });
            return;
        }

        let indices = self.visible_repos();
        let mut want_detail: Option<String> = None;
        let mut want_terminal: Option<String> = None;
        let mut want_copy: Option<String> = None;
        let profile = self.cfg.current().clone();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for i in indices {
                    let repo = &self.repos[i];
                    let selected = self.selected == Some(i);
                    let resp = ui.push_id(i, |ui| {
                        egui::Frame::group(ui.style())
                            .fill(if selected {
                                ui.visuals().selection.bg_fill.gamma_multiply(0.35)
                            } else {
                                ui.visuals().faint_bg_color
                            })
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width() - 8.0);
                                ui.horizontal(|ui| {
                                    ui.strong(&repo.name);
                                    ui.small(if repo.bare { "bare" } else { "working copy" });
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui.small_button("SSH here").clicked() {
                                                let dir = if repo.bare {
                                                    repo.path.clone()
                                                } else {
                                                    repo.path
                                                        .strip_suffix("/.git")
                                                        .unwrap_or(&repo.path)
                                                        .to_string()
                                                };
                                                want_terminal = Some(dir);
                                            }
                                            if ui.small_button("Copy URL").clicked() {
                                                want_copy = Some(terminal::clone_url(
                                                    &profile, &repo.path,
                                                ));
                                            }
                                            if ui.small_button("Details").clicked() {
                                                want_detail = Some(repo.path.clone());
                                            }
                                        },
                                    );
                                });
                                ui.small(&repo.path);
                                if !repo.description.is_empty() {
                                    ui.small(&repo.description);
                                }
                                match &repo.last {
                                    Some(c) => {
                                        ui.horizontal(|ui| {
                                            ui.monospace(&c.hash);
                                            ui.small(&c.when);
                                            ui.small("|");
                                            ui.small(&c.subject);
                                        });
                                    }
                                    None if self.no_git => {
                                        ui.small("commit unknown - git missing on server");
                                    }
                                    None => {
                                        ui.small("no commits yet (open Details for why)");
                                    }
                                }
                            });
                    });
                    if resp.response.interact(egui::Sense::click()).clicked() {
                        self.selected = Some(i);
                        want_detail = Some(self.repos[i].path.clone());
                    }
                }
            });

        if let Some(url) = want_copy {
            ui.ctx().copy_text(url.clone());
            self.push_log(format!("copied {url}"));
        }
        if let Some(path) = want_detail {
            self.selected = self.repos.iter().position(|r| r.path == path);
            let _ = self.cmd_tx.send(Cmd::Detail { path });
        }
        if let Some(dir) = want_terminal {
            self.open_terminal(Some(dir));
        }
    }

    fn new_repo_window(&mut self, ctx: &egui::Context) {
        let Some(mut dlg) = self.new_repo.take() else {
            return;
        };
        let roots = self.cfg.current().repo_roots.clone();
        let mut open = true;
        let mut submit = false;
        let mut cancel = false;

        egui::Window::new("New repository")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_min_width(420.0);
                egui::Grid::new("new_repo_form")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Root");
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_salt("new_repo_root")
                                .selected_text(dlg.root.clone())
                                .show_ui(ui, |ui| {
                                    for r in &roots {
                                        ui.selectable_value(&mut dlg.root, r.clone(), r);
                                    }
                                });
                            ui.add(
                                egui::TextEdit::singleline(&mut dlg.root).desired_width(200.0),
                            );
                        });
                        ui.end_row();

                        ui.label("Name");
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut dlg.name)
                                .hint_text("my-project")
                                .desired_width(260.0),
                        );
                        submit |= resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.end_row();

                        ui.label("Description");
                        ui.add(
                            egui::TextEdit::singleline(&mut dlg.description)
                                .hint_text("optional")
                                .desired_width(260.0),
                        );
                        ui.end_row();
                    });

                ui.checkbox(
                    &mut dlg.shared,
                    "Shared with the owning group (git init --shared=group)",
                );

                let preview = ssh::validate_repo_name(&dlg.name)
                    .map(|dir| format!("{}/{dir}", dlg.root.trim_end_matches('/')))
                    .unwrap_or_default();
                if !preview.is_empty() {
                    ui.small(format!("will create {preview}"));
                }
                if !dlg.error.is_empty() {
                    ui.colored_label(egui::Color32::from_rgb(220, 100, 100), &dlg.error);
                }

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    submit |= ui.button("Create").clicked();
                    cancel |= ui.button("Cancel").clicked();
                });
            });

        if cancel || !open {
            return;
        }
        if submit {
            match ssh::validate_repo_name(&dlg.name) {
                Ok(_) => {
                    self.status = "creating repository...".to_string();
                    let _ = self.cmd_tx.send(Cmd::CreateRepo {
                        root: dlg.root.clone(),
                        name: dlg.name.clone(),
                        description: dlg.description.clone(),
                        shared: dlg.shared,
                    });
                    dlg.error.clear();
                }
                Err(e) => dlg.error = e.to_string(),
            }
        }
        self.new_repo = Some(dlg);
    }

    fn log_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Details & log");
            if ui.small_button("Clear log").clicked() {
                self.log.clear();
                self.output = None;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let connected = self.conn == Conn::Up;
                let run = ui.add_enabled(connected, egui::Button::new("Run")).clicked();
                let entered = ui
                    .add_enabled(
                        connected,
                        egui::TextEdit::singleline(&mut self.exec_input)
                            .hint_text("remote command, e.g. git --version")
                            .desired_width(320.0),
                    )
                    .lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (run || entered) && !self.exec_input.trim().is_empty() {
                    let script = self.exec_input.trim().to_string();
                    self.push_log(format!("$ {script}"));
                    let _ = self.cmd_tx.send(Cmd::Exec {
                        label: script.clone(),
                        script,
                    });
                }
            });
        });
        ui.separator();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if let Some((path, d)) = &self.detail {
                    ui.strong(path);
                    if !d.size.is_empty() {
                        ui.label(format!("size: {}  |  HEAD: {}", d.size, d.head));
                    }
                    if !d.branches.is_empty() {
                        ui.label(format!("branches: {}", d.branches.join(", ")));
                    }
                    if !d.git.is_empty() {
                        ui.small(&d.git);
                    }
                    if !d.error.is_empty() {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 140, 90),
                            format!("git said: {}", d.error),
                        );
                    }
                    if d.commits.is_empty() {
                        if d.error.is_empty() {
                            ui.label(if d.branches.is_empty() {
                                "no commits: the repository has no branches yet"
                            } else {
                                "no commits reachable from HEAD or any branch"
                            });
                        }
                    } else {
                        let hint = if d.truncated {
                            format!("{} most recent commits", d.commits.len())
                        } else {
                            format!("{} commits", d.commits.len())
                        };
                        ui.label(hint);
                        egui::Grid::new("commits")
                            .num_columns(4)
                            .striped(true)
                            .spacing([12.0, 2.0])
                            .show(ui, |ui| {
                                for c in &d.commits {
                                    let resp = ui.monospace(&c.hash);
                                    if resp
                                        .interact(egui::Sense::click())
                                        .on_hover_text("click to copy the full line")
                                        .clicked()
                                    {
                                        ui.ctx().copy_text(format!(
                                            "{} {} {} {}",
                                            c.hash, c.date, c.author, c.subject
                                        ));
                                    }
                                    ui.label(&c.when)
                                        .on_hover_text(&c.date);
                                    ui.label(&c.author);
                                    ui.label(&c.subject);
                                    ui.end_row();
                                }
                            });
                    }
                    ui.separator();
                }
                if let Some((label, text)) = &self.output {
                    ui.strong(label);
                    ui.monospace(text);
                    ui.separator();
                }
                for line in &self.log {
                    ui.monospace(line);
                }
            });
    }
}
