use crate::fsutil::{ReplaceOutcome, atomic_replace_text};
use crate::settings::Settings;
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;
use time::OffsetDateTime;
use time::macros::format_description;
use toml_edit::{DocumentMut, InlineTable, Item, TableLike, Value};

pub const BRIDGE_HEADER: &str = "X-Headroom-Base-Url";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RouteKind {
    Missing,
    Invalid,
    Direct,
    CcSwitch,
    Bridged,
}

impl fmt::Display for RouteKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Missing => "missing",
            Self::Invalid => "invalid",
            Self::Direct => "direct",
            Self::CcSwitch => "cc-switch",
            Self::Bridged => "bridged",
        })
    }
}

#[derive(Clone, Debug)]
pub struct ConfigRoute {
    pub provider: Option<String>,
    pub route: RouteKind,
}

impl ConfigRoute {
    fn new(route: RouteKind) -> Self {
        Self {
            provider: None,
            route,
        }
    }
}

pub fn config_route(settings: &Settings) -> ConfigRoute {
    if !settings.config_path.exists() {
        return ConfigRoute::new(RouteKind::Missing);
    }
    parse_route(settings).unwrap_or_else(|_| ConfigRoute::new(RouteKind::Invalid))
}

fn parse_route(settings: &Settings) -> Result<ConfigRoute> {
    let text = fs::read_to_string(&settings.config_path)?;
    let doc = text.parse::<DocumentMut>()?;
    let provider = doc
        .get("model_provider")
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_owned();
    let table = doc
        .get("model_providers")
        .and_then(Item::as_table_like)
        .and_then(|providers| providers.get(&provider))
        .and_then(Item::as_table_like);
    let Some(table) = table else {
        let mut result = ConfigRoute::new(RouteKind::Invalid);
        result.provider = (!provider.is_empty()).then_some(provider);
        return Ok(result);
    };
    let base_url = table
        .get("base_url")
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_owned();
    let headers = table.get("http_headers").and_then(Item::as_table_like);
    let header = header_key(headers);
    let upstream = header
        .as_deref()
        .and_then(|key| headers.and_then(|items| items.get(key)))
        .and_then(Item::as_str)
        .map(str::to_owned);
    let route = if same_url(&base_url, &settings.headroom_base())
        && same_url(
            upstream.as_deref().unwrap_or_default(),
            &settings.cc_origin(),
        ) {
        RouteKind::Bridged
    } else if same_url(&base_url, &settings.cc_base()) {
        RouteKind::CcSwitch
    } else {
        RouteKind::Direct
    };
    Ok(ConfigRoute {
        provider: Some(provider),
        route,
    })
}

pub fn reconcile(settings: &Settings, bridge: bool) -> Result<bool> {
    fs::create_dir_all(&settings.state_dir)?;
    let lock_path = settings.state_dir.join("config.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    FileExt::lock_exclusive(&lock)?;

    for attempt in 0..3 {
        let original = fs::read_to_string(&settings.config_path)
            .with_context(|| format!("failed to read {}", settings.config_path.display()))?;
        let mut doc = original.parse::<DocumentMut>()?;
        update_route(&mut doc, settings, bridge)?;
        let updated = doc.to_string();
        let mode = fs::metadata(&settings.config_path)?.permissions().mode();
        match atomic_replace_text(&settings.config_path, Some(&original), &updated, mode)? {
            ReplaceOutcome::Unchanged => return Ok(false),
            ReplaceOutcome::Replaced => return Ok(true),
            ReplaceOutcome::Conflict if attempt < 2 => thread::sleep(Duration::from_millis(100)),
            ReplaceOutcome::Conflict => bail!("config changed during reconciliation"),
        }
    }
    Ok(false)
}

fn update_route(doc: &mut DocumentMut, settings: &Settings, bridge: bool) -> Result<()> {
    let provider = doc
        .get("model_provider")
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_owned();
    let table = doc
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
        .and_then(|providers| providers.get_mut(&provider))
        .and_then(Item::as_table_like_mut);
    let Some(table) = table else {
        bail!("active Codex model provider is not configurable");
    };

    let current_base = table
        .get("base_url")
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_owned();
    let header = header_key(table.get("http_headers").and_then(Item::as_table_like));

    if bridge {
        if !same_url(&current_base, &settings.cc_base())
            && !same_url(&current_base, &settings.headroom_base())
        {
            bail!(
                "refusing to bridge unexpected provider URL: {}",
                if current_base.is_empty() {
                    "<empty>"
                } else {
                    &current_base
                }
            );
        }
        set_value(table, "base_url", Value::from(settings.headroom_base()));
        set_value(table, "supports_websockets", Value::from(false));
        if table.get("http_headers").is_none() {
            table.insert(
                "http_headers",
                Item::Value(Value::InlineTable(InlineTable::new())),
            );
        }
        let headers = table
            .get_mut("http_headers")
            .and_then(Item::as_table_like_mut)
            .context("active provider http_headers is not a table")?;
        set_value(
            headers,
            header.as_deref().unwrap_or(BRIDGE_HEADER),
            Value::from(settings.cc_origin()),
        );
    } else {
        if !same_url(&current_base, &settings.headroom_base()) && header.is_none() {
            return Ok(());
        }
        set_value(table, "base_url", Value::from(settings.cc_base()));
        set_value(table, "supports_websockets", Value::from(false));
        if let Some(header) = header
            && let Some(headers) = table
                .get_mut("http_headers")
                .and_then(Item::as_table_like_mut)
        {
            headers.remove(&header);
            if headers.is_empty() {
                table.remove("http_headers");
            }
        }
    }
    Ok(())
}

fn set_value(table: &mut dyn TableLike, key: &str, mut updated: Value) {
    if let Some(current) = table.get(key).and_then(Item::as_value) {
        *updated.decor_mut() = current.decor().clone();
    }
    table.insert(key, Item::Value(updated));
}

fn header_key(headers: Option<&dyn TableLike>) -> Option<String> {
    headers?.iter().find_map(|(key, _)| {
        key.eq_ignore_ascii_case(BRIDGE_HEADER)
            .then(|| key.to_owned())
    })
}

fn same_url(left: &str, right: &str) -> bool {
    left.trim_end_matches('/')
        .eq_ignore_ascii_case(right.trim_end_matches('/'))
}

pub fn snapshot(settings: &Settings) -> Result<PathBuf> {
    let backup_dir = settings.state_dir.join("backups");
    fs::create_dir_all(&backup_dir)?;
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    let stamp = now.format(format_description!(
        "[year][month][day]-[hour][minute][second]"
    ))?;
    for suffix in 0..1000 {
        let name = if suffix == 0 {
            format!("config-{stamp}.toml")
        } else {
            format!("config-{stamp}-{suffix}.toml")
        };
        let target = backup_dir.join(name);
        let mut target_file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("failed to create config backup"),
        };
        let copy_result = (|| -> Result<()> {
            let mut source = fs::File::open(&settings.config_path)?;
            io::copy(&mut source, &mut target_file)?;
            target_file.flush()?;
            target_file.set_permissions(fs::metadata(&settings.config_path)?.permissions())?;
            target_file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = copy_result {
            let _ = fs::remove_file(&target);
            return Err(error).context("failed to back up Codex config");
        }
        return Ok(target);
    }
    bail!("too many config backups created in one second")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, Settings) {
        let temp = TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let settings = Settings {
            home: root.clone(),
            config_path: root.join("config.toml"),
            cc_db_path: root.join("cc-switch.db"),
            state_dir: root.join("state"),
            launch_agents_dir: root.join("LaunchAgents"),
            headroom_host: "127.0.0.1",
            headroom_port: 8787,
            cc_host: "127.0.0.1",
            cc_port: 15721,
        };
        fs::write(
            &settings.config_path,
            r#"# keep this comment
model_provider = "custom"
model = "gpt-5.6-sol"

[model_providers.custom]
name = "CCTQ"
base_url = "http://127.0.0.1:15721/v1" # keep endpoint comment
wire_api = "responses"
experimental_bearer_token = "PROXY_MANAGED"

[features]
memories = true
"#,
        )
        .unwrap();
        (temp, settings)
    }

    #[test]
    fn bridge_preserves_auth_comments_and_unrelated_config() {
        let (_temp, settings) = fixture();
        assert!(reconcile(&settings, true).unwrap());
        let text = fs::read_to_string(&settings.config_path).unwrap();
        assert!(text.contains("# keep this comment"));
        assert!(text.contains("# keep endpoint comment"));
        assert!(text.contains("experimental_bearer_token = \"PROXY_MANAGED\""));
        assert!(text.contains("memories = true"));
        assert_eq!(config_route(&settings).route, RouteKind::Bridged);
        assert!(!reconcile(&settings, true).unwrap());
    }

    #[test]
    fn bypass_removes_only_bridge_header() {
        let (_temp, settings) = fixture();
        reconcile(&settings, true).unwrap();
        assert!(reconcile(&settings, false).unwrap());
        let text = fs::read_to_string(&settings.config_path).unwrap();
        assert!(!text.contains(BRIDGE_HEADER));
        assert!(text.contains("experimental_bearer_token = \"PROXY_MANAGED\""));
        assert_eq!(config_route(&settings).route, RouteKind::CcSwitch);
    }

    #[test]
    fn refuses_unexpected_provider_url() {
        let (_temp, settings) = fixture();
        let text = fs::read_to_string(&settings.config_path)
            .unwrap()
            .replace("http://127.0.0.1:15721/v1", "https://provider.example/v1");
        fs::write(&settings.config_path, text).unwrap();
        let error = reconcile(&settings, true).unwrap_err().to_string();
        assert!(error.contains("unexpected provider URL"));
    }
}
