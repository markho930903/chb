use crate::fsutil::atomic_replace_text;
use anyhow::{Context, Result, bail};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, value};

const MANIFEST_NAME: &str = "settings.toml";

#[derive(Debug, Default)]
pub struct Overrides {
    pub config_path: Option<PathBuf>,
    pub cc_db_path: Option<PathBuf>,
    pub headroom_port: Option<u16>,
    pub cc_port: Option<u16>,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub home: PathBuf,
    pub config_path: PathBuf,
    pub cc_db_path: PathBuf,
    pub state_dir: PathBuf,
    pub launch_agents_dir: PathBuf,
    pub headroom_host: &'static str,
    pub headroom_port: u16,
    pub cc_host: &'static str,
    pub cc_port: u16,
}

impl Settings {
    pub fn load(overrides: Overrides) -> Result<Self> {
        let Overrides {
            config_path,
            cc_db_path,
            headroom_port,
            cc_port,
        } = overrides;
        let system_home = env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?;
        let home = env::var_os("CODEX_HEADROOM_BRIDGE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| system_home.clone());
        let home = absolute(expand_tilde(home, &system_home))?;
        let state_dir = env::var_os("CODEX_HEADROOM_BRIDGE_STATE")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state/codex-headroom-bridge"));
        let state_dir = absolute(expand_tilde(state_dir, &home))?;
        let manifest = read_manifest(&state_dir.join(MANIFEST_NAME))?;

        let config_path = if let Some(path) = config_path {
            path
        } else if let Some(path) = env::var_os("CODEX_HEADROOM_BRIDGE_CONFIG") {
            PathBuf::from(path)
        } else {
            manifest_path(&manifest, "config_path")?
                .unwrap_or_else(|| home.join(".codex/config.toml"))
        };
        let cc_db_path = if let Some(path) = cc_db_path {
            path
        } else if let Some(path) = env::var_os("CODEX_HEADROOM_BRIDGE_CC_DB") {
            PathBuf::from(path)
        } else {
            manifest_path(&manifest, "cc_db_path")?
                .unwrap_or_else(|| home.join(".cc-switch/cc-switch.db"))
        };

        let headroom_port = if let Some(port) = headroom_port {
            port
        } else if let Some(port) = env_port("CODEX_HEADROOM_BRIDGE_HEADROOM_PORT")? {
            port
        } else {
            manifest_port(&manifest, "headroom_port")?.unwrap_or(8787)
        };
        let cc_port = if let Some(port) = cc_port {
            port
        } else if let Some(port) = env_port("CODEX_HEADROOM_BRIDGE_CC_PORT")? {
            port
        } else {
            manifest_port(&manifest, "cc_port")?.unwrap_or(15721)
        };

        Ok(Self {
            home: home.clone(),
            config_path: absolute(expand_tilde(config_path, &home))?,
            cc_db_path: absolute(expand_tilde(cc_db_path, &home))?,
            state_dir,
            launch_agents_dir: home.join("Library/LaunchAgents"),
            headroom_host: "127.0.0.1",
            headroom_port,
            cc_host: "127.0.0.1",
            cc_port,
        })
    }

    pub fn headroom_base(&self) -> String {
        format!("http://{}:{}/v1", self.headroom_host, self.headroom_port)
    }

    pub fn headroom_origin(&self) -> String {
        format!("http://{}:{}", self.headroom_host, self.headroom_port)
    }

    pub fn cc_base(&self) -> String {
        format!("http://{}:{}/v1", self.cc_host, self.cc_port)
    }

    pub fn cc_origin(&self) -> String {
        format!("http://{}:{}", self.cc_host, self.cc_port)
    }

    pub fn save(&self) -> Result<()> {
        let mut doc = DocumentMut::new();
        doc["version"] = value(1);
        doc["config_path"] = value(path_text(&self.config_path)?);
        doc["cc_db_path"] = value(path_text(&self.cc_db_path)?);
        doc["headroom_port"] = value(i64::from(self.headroom_port));
        doc["cc_port"] = value(i64::from(self.cc_port));
        atomic_replace_text(&self.manifest_path(), None, &doc.to_string(), 0o600)?;
        Ok(())
    }

    pub fn remove_manifest(&self) -> Result<()> {
        match fs::remove_file(self.manifest_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("failed to remove settings manifest"),
        }
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.state_dir.join(MANIFEST_NAME)
    }
}

fn read_manifest(path: &Path) -> Result<Option<DocumentMut>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("failed to read settings manifest"),
    };
    let doc = text
        .parse::<DocumentMut>()
        .context("settings manifest is invalid TOML")?;
    if doc.get("version").and_then(|item| item.as_integer()) != Some(1) {
        bail!("unsupported settings manifest version");
    }
    Ok(Some(doc))
}

fn manifest_path(doc: &Option<DocumentMut>, key: &str) -> Result<Option<PathBuf>> {
    let Some(item) = doc.as_ref().and_then(|doc| doc.get(key)) else {
        return Ok(None);
    };
    item.as_str()
        .map(PathBuf::from)
        .map(Some)
        .with_context(|| format!("settings manifest {key} must be a string"))
}

fn manifest_port(doc: &Option<DocumentMut>, key: &str) -> Result<Option<u16>> {
    let Some(item) = doc.as_ref().and_then(|doc| doc.get(key)) else {
        return Ok(None);
    };
    let port = item
        .as_integer()
        .and_then(|value| u16::try_from(value).ok())
        .filter(|port| *port != 0)
        .with_context(|| format!("settings manifest {key} must be a port from 1 to 65535"))?;
    Ok(Some(port))
}

fn env_port(key: &str) -> Result<Option<u16>> {
    match env::var(key) {
        Ok(value) => value
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .map(Some)
            .with_context(|| format!("{key} must be a port from 1 to 65535")),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {key}")),
    }
}

fn expand_tilde(path: PathBuf, home: &Path) -> PathBuf {
    if path == Path::new("~") {
        return home.to_path_buf();
    }
    path.strip_prefix("~/")
        .map(|suffix| home.join(suffix))
        .unwrap_or(path)
}

fn absolute(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};
    use tempfile::TempDir;

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn saved_settings_are_reused() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        // SAFETY: this test serializes all environment access in this module.
        unsafe {
            env::set_var("CODEX_HEADROOM_BRIDGE_HOME", temp.path());
        }
        let first = Settings::load(Overrides {
            headroom_port: Some(9898),
            cc_port: Some(16161),
            ..Overrides::default()
        })
        .unwrap();
        first.save().unwrap();
        let loaded = Settings::load(Overrides::default()).unwrap();
        assert_eq!(loaded.headroom_port, 9898);
        assert_eq!(loaded.cc_port, 16161);
        // SAFETY: this test serializes all environment access in this module.
        unsafe {
            env::remove_var("CODEX_HEADROOM_BRIDGE_HOME");
        }
    }
}
