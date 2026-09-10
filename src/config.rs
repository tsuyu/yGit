use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthKind {
    Key,
    Password,
}

impl Default for AuthKind {
    fn default() -> Self {
        AuthKind::Key
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: AuthKind,
    /// Path to a private key. Empty means "try the usual ~/.ssh candidates".
    pub key_path: String,
    /// Directories on the NAS that are scanned for git repositories.
    pub repo_roots: Vec<String>,
    /// Refuse to connect when the host key is unknown or changed.
    pub strict_host_key: bool,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            name: "synology".to_string(),
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: AuthKind::Key,
            key_path: String::new(),
            repo_roots: vec![
                "/volume1/git".to_string(),
                "/volume1/homes".to_string(),
            ],
            strict_host_key: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub profiles: Vec<Profile>,
    pub selected: usize,
}

impl Config {
    pub fn path() -> PathBuf {
        let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        base.join("ygit").join("config.json")
    }

    pub fn load() -> Self {
        let path = Self::path();
        let mut cfg: Config = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if cfg.profiles.is_empty() {
            cfg.profiles.push(Profile::default());
        }
        if cfg.selected >= cfg.profiles.len() {
            cfg.selected = 0;
        }
        cfg
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn current(&self) -> &Profile {
        &self.profiles[self.selected.min(self.profiles.len() - 1)]
    }

    pub fn current_mut(&mut self) -> &mut Profile {
        let idx = self.selected.min(self.profiles.len() - 1);
        &mut self.profiles[idx]
    }
}

/// Private keys tried in order when the profile does not name one.
pub fn default_key_candidates() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    ["id_ed25519", "id_ecdsa", "id_rsa"]
        .iter()
        .map(|name| home.join(".ssh").join(name))
        .filter(|p| p.exists())
        .collect()
}
