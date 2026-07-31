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
    pub upstream: Option<String>,
}

impl ConfigRoute {
    fn new(route: RouteKind) -> Self {
        Self {
            provider: None,
            route,
            upstream: None,
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
    let route = if same_url(&base_url, &settings.headroom_base()) {
        if upstream
            .as_deref()
            .is_some_and(|url| usable_upstream(url, settings))
        {
            RouteKind::Bridged
        } else {
            RouteKind::Invalid
        }
    } else if same_url(&base_url, &settings.cc_base()) {
        RouteKind::CcSwitch
    } else {
        RouteKind::Direct
    };
    Ok(ConfigRoute {
        provider: Some(provider),
        route,
        upstream: if same_url(&base_url, &settings.headroom_base()) {
            upstream
        } else if base_url.is_empty() {
            None
        } else {
            Some(base_url)
        },
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
        if same_url(&current_base, &settings.headroom_base()) {
            let upstream = header
                .as_deref()
                .and_then(|key| table.get("http_headers")?.as_table_like()?.get(key))
                .and_then(Item::as_str)
                .context("Headroom route has no preserved provider URL")?;
            if usable_upstream(upstream, settings) {
                return Ok(());
            }
            bail!("preserved provider URL cannot be empty or point to Headroom");
        }
        if current_base.is_empty() {
            bail!("refusing to bridge an active provider without a base URL");
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
            Value::from(current_base),
        );
    } else {
        if !same_url(&current_base, &settings.headroom_base()) {
            return Ok(());
        }
        let Some(header) = header else {
            bail!("Headroom route has no preserved provider URL");
        };
        let upstream = table
            .get("http_headers")
            .and_then(Item::as_table_like)
            .and_then(|headers| headers.get(&header))
            .and_then(Item::as_str)
            .context("Headroom route has no preserved provider URL")?;
        if !usable_upstream(upstream, settings) {
            bail!("preserved provider URL cannot be empty or point to Headroom");
        }
        let upstream = upstream.to_owned();
        set_value(table, "base_url", Value::from(upstream));
        set_value(table, "supports_websockets", Value::from(false));
        if let Some(headers) = table
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

fn usable_upstream(url: &str, settings: &Settings) -> bool {
    !url.trim().is_empty() && !same_url(url, &settings.headroom_base())
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
            web_host: "127.0.0.1",
            web_port: 8788,
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
base_url = "https://provider.example/v1" # keep endpoint comment
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
        assert_eq!(
            config_route(&settings).upstream.as_deref(),
            Some("https://provider.example/v1")
        );
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
        assert!(text.contains("base_url = \"https://provider.example/v1\""));
        assert_eq!(config_route(&settings).route, RouteKind::Direct);
    }

    #[test]
    fn cc_switch_proxy_is_preserved_as_headroom_upstream() {
        let (_temp, settings) = fixture();
        let text = fs::read_to_string(&settings.config_path)
            .unwrap()
            .replace("https://provider.example/v1", "http://127.0.0.1:15721/v1");
        fs::write(&settings.config_path, text).unwrap();
        assert!(reconcile(&settings, true).unwrap());
        let route = config_route(&settings);
        assert_eq!(route.route, RouteKind::Bridged);
        assert_eq!(route.upstream.as_deref(), Some("http://127.0.0.1:15721/v1"));

        assert!(reconcile(&settings, false).unwrap());
        let text = fs::read_to_string(&settings.config_path).unwrap();
        assert!(text.contains("base_url = \"http://127.0.0.1:15721/v1\""));
        assert!(!text.contains(BRIDGE_HEADER));
    }

    #[test]
    fn provider_switch_is_captured_after_headroom_is_enabled() {
        let (_temp, settings) = fixture();
        reconcile(&settings, true).unwrap();
        fs::write(
            &settings.config_path,
            r#"model_provider = "custom"

[model_providers.custom]
base_url = "https://another-provider.example/v1"
wire_api = "responses"
"#,
        )
        .unwrap();
        assert!(reconcile(&settings, true).unwrap());
        let text = fs::read_to_string(&settings.config_path).unwrap();
        assert!(text.contains("base_url = \"http://127.0.0.1:8787/v1\""));
        assert!(text.contains("X-Headroom-Base-Url = \"https://another-provider.example/v1\""));
    }

    #[test]
    fn rejects_headroom_upstream_loop() {
        let (_temp, settings) = fixture();
        let text = fs::read_to_string(&settings.config_path)
            .unwrap()
            .replace("https://provider.example/v1", "http://127.0.0.1:8787/v1")
            .replace(
                "wire_api = \"responses\"",
                "wire_api = \"responses\"\nhttp_headers = { X-Headroom-Base-Url = \"http://127.0.0.1:8787/v1\" }",
            );
        fs::write(&settings.config_path, text).unwrap();
        assert_eq!(config_route(&settings).route, RouteKind::Invalid);
        let error = reconcile(&settings, true).unwrap_err().to_string();
        assert!(error.contains("point to Headroom"));
    }
}
