use crate::config::{ConfigRoute, RouteKind, config_route, reconcile, snapshot};
use crate::fsutil::atomic_replace_text;
use crate::settings::Settings;
use anyhow::{Context, Result, bail};
use plist::{Dictionary, Value};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use signal_hook::consts::{SIGINT, SIGTERM};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub const PROXY_LABEL: &str = "ai.headroom.codex-ccswitch.proxy";
pub const BRIDGE_LABEL: &str = "ai.headroom.codex-ccswitch.bridge";

pub struct RuntimeStatus {
    pub headroom_ready: bool,
    pub cc_switch_ready: bool,
    pub cc_switch_takeover: bool,
    pub proxy_service_loaded: bool,
    pub bridge_service_loaded: bool,
    pub config: ConfigRoute,
}

impl RuntimeStatus {
    pub fn healthy(&self) -> bool {
        self.headroom_ready
            && self.cc_switch_ready
            && self.cc_switch_takeover
            && self.proxy_service_loaded
            && self.bridge_service_loaded
            && self.config.route == RouteKind::Bridged
    }
}

pub fn tcp_ready(host: &str, port: u16, timeout: Duration) -> bool {
    (host, port)
        .to_socket_addrs()
        .ok()
        .into_iter()
        .flatten()
        .any(|address| TcpStream::connect_timeout(&address, timeout).is_ok())
}

fn http_success(host: &str, port: u16, path: &str, timeout: Duration) -> bool {
    let addresses = match (host, port).to_socket_addrs() {
        Ok(addresses) => addresses,
        Err(_) => return false,
    };
    for address in addresses {
        let Ok(mut stream) = TcpStream::connect_timeout(&address, timeout) else {
            continue;
        };
        let _ = stream.set_read_timeout(Some(timeout));
        let _ = stream.set_write_timeout(Some(timeout));
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
        if stream.write_all(request.as_bytes()).is_err() {
            continue;
        }
        let mut status_line = String::new();
        if BufReader::new(stream).read_line(&mut status_line).is_err() {
            continue;
        }
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<u16>().ok());
        if matches!(status, Some(200..=299)) {
            return true;
        }
    }
    false
}

pub fn headroom_ready(settings: &Settings) -> bool {
    http_success(
        settings.headroom_host,
        settings.headroom_port,
        "/readyz",
        Duration::from_millis(800),
    )
}

fn dashboard_ready(settings: &Settings) -> bool {
    http_success(
        settings.headroom_host,
        settings.headroom_port,
        "/dashboard",
        Duration::from_millis(800),
    )
}

pub fn cc_takeover_enabled(settings: &Settings) -> bool {
    if !settings.cc_db_path.exists() {
        return false;
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let result = (|| -> rusqlite::Result<bool> {
        let connection = Connection::open_with_flags(&settings.cc_db_path, flags)?;
        connection.busy_timeout(Duration::from_millis(500))?;
        let enabled = connection
            .query_row(
                "SELECT enabled FROM proxy_config WHERE app_type = 'codex'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        Ok(enabled.is_some_and(|value| value != 0))
    })();
    result.unwrap_or(false)
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and no side effects.
    unsafe { libc::getuid() }
}

fn launch_target(label: &str) -> String {
    format!("gui/{}/{label}", uid())
}

fn launchctl(args: &[String], check: bool) -> Result<Output> {
    let output = Command::new("/bin/launchctl")
        .args(args)
        .output()
        .context("failed to run launchctl")?;
    if check && !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!(
            "launchctl {} failed: {}",
            args.first().map(String::as_str).unwrap_or("command"),
            if message.is_empty() {
                output.status.to_string()
            } else {
                message
            }
        );
    }
    Ok(output)
}

fn plist_path(settings: &Settings, label: &str) -> PathBuf {
    settings.launch_agents_dir.join(format!("{label}.plist"))
}

pub fn service_loaded(label: &str) -> bool {
    launchctl(&["print".into(), launch_target(label)], false)
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn path_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}

fn string_array(values: &[String]) -> Value {
    Value::Array(values.iter().cloned().map(Value::String).collect())
}

fn add_common(data: &mut Dictionary, settings: &Settings) -> Result<()> {
    data.insert("RunAtLoad".into(), Value::Boolean(true));
    data.insert("KeepAlive".into(), Value::Boolean(true));
    data.insert("ProcessType".into(), Value::String("Background".into()));
    data.insert("ThrottleInterval".into(), Value::Integer(5.into()));
    data.insert(
        "WorkingDirectory".into(),
        Value::String(path_string(&settings.home)?),
    );
    Ok(())
}

fn proxy_plist(settings: &Settings, headroom_bin: &Path) -> Result<Value> {
    let mut data = Dictionary::new();
    data.insert("Label".into(), Value::String(PROXY_LABEL.into()));
    data.insert(
        "ProgramArguments".into(),
        string_array(&[
            path_string(headroom_bin)?,
            "proxy".into(),
            "--host".into(),
            settings.headroom_host.into(),
            "--port".into(),
            settings.headroom_port.to_string(),
        ]),
    );
    let mut environment = Dictionary::new();
    environment.insert("HEADROOM_TELEMETRY".into(), Value::String("off".into()));
    environment.insert(
        "HEADROOM_STRIP_INTERNAL_HEADERS".into(),
        Value::String("enabled".into()),
    );
    data.insert(
        "EnvironmentVariables".into(),
        Value::Dictionary(environment),
    );
    data.insert(
        "StandardOutPath".into(),
        Value::String(path_string(&settings.state_dir.join("headroom.log"))?),
    );
    data.insert(
        "StandardErrorPath".into(),
        Value::String(path_string(&settings.state_dir.join("headroom.err.log"))?),
    );
    add_common(&mut data, settings)?;
    Ok(Value::Dictionary(data))
}

fn bridge_plist(settings: &Settings, bridge_bin: &Path) -> Result<Value> {
    let mut data = Dictionary::new();
    data.insert("Label".into(), Value::String(BRIDGE_LABEL.into()));
    data.insert(
        "ProgramArguments".into(),
        string_array(&[path_string(bridge_bin)?, "watch".into()]),
    );
    let mut environment = Dictionary::new();
    environment.insert(
        "CODEX_HEADROOM_BRIDGE_HOME".into(),
        Value::String(path_string(&settings.home)?),
    );
    environment.insert(
        "CODEX_HEADROOM_BRIDGE_STATE".into(),
        Value::String(path_string(&settings.state_dir)?),
    );
    data.insert(
        "EnvironmentVariables".into(),
        Value::Dictionary(environment),
    );
    data.insert(
        "StandardOutPath".into(),
        Value::String(path_string(&settings.state_dir.join("bridge.log"))?),
    );
    data.insert(
        "StandardErrorPath".into(),
        Value::String(path_string(&settings.state_dir.join("bridge.err.log"))?),
    );
    add_common(&mut data, settings)?;
    Ok(Value::Dictionary(data))
}

fn write_plist(path: &Path, value: &Value) -> Result<()> {
    let mut bytes = Vec::new();
    plist::to_writer_xml(&mut bytes, value)?;
    let text = String::from_utf8(bytes).context("plist encoder produced invalid UTF-8")?;
    atomic_replace_text(path, None, &text, 0o644)?;
    Ok(())
}

fn bootout(label: &str) {
    let _ = launchctl(&["bootout".into(), launch_target(label)], false);
}

fn bootstrap(settings: &Settings, label: &str) -> Result<()> {
    let domain = format!("gui/{}", uid());
    let path = path_string(&plist_path(settings, label))?;
    launchctl(&["bootstrap".into(), domain, path], true)?;
    Ok(())
}

pub fn install_services(
    settings: &Settings,
    headroom_bin: &Path,
    bridge_bin: &Path,
) -> Result<PathBuf> {
    fs::create_dir_all(&settings.state_dir)?;
    let backup = snapshot(settings)?;
    settings.save()?;

    let services = [
        (PROXY_LABEL, proxy_plist(settings, headroom_bin)?),
        (BRIDGE_LABEL, bridge_plist(settings, bridge_bin)?),
    ];
    for (label, value) in services {
        bootout(label);
        write_plist(&plist_path(settings, label), &value)?;
        bootstrap(settings, label)?;
    }
    Ok(backup)
}

pub fn start_services(settings: &Settings) -> Result<()> {
    for label in [PROXY_LABEL, BRIDGE_LABEL] {
        let path = plist_path(settings, label);
        if !path.exists() {
            bail!("service is not installed: {label}");
        }
        if service_loaded(label) {
            launchctl(
                &["kickstart".into(), "-k".into(), launch_target(label)],
                true,
            )?;
        } else {
            bootstrap(settings, label)?;
        }
    }
    Ok(())
}

pub fn stop_services(settings: &Settings) -> Result<()> {
    bootout(BRIDGE_LABEL);
    if settings.config_path.exists() {
        reconcile(settings, false)
            .context("could not restore direct CC Switch routing; Headroom was left running")?;
    }
    bootout(PROXY_LABEL);
    Ok(())
}

pub fn uninstall_services(settings: &Settings) -> Result<()> {
    stop_services(settings)?;
    for label in [BRIDGE_LABEL, PROXY_LABEL] {
        match fs::remove_file(plist_path(settings, label)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("failed to remove LaunchAgent"),
        }
    }
    settings.remove_manifest()?;
    Ok(())
}

pub fn status(settings: &Settings) -> RuntimeStatus {
    RuntimeStatus {
        headroom_ready: headroom_ready(settings),
        cc_switch_ready: tcp_ready(
            settings.cc_host,
            settings.cc_port,
            Duration::from_millis(400),
        ),
        cc_switch_takeover: cc_takeover_enabled(settings),
        proxy_service_loaded: service_loaded(PROXY_LABEL),
        bridge_service_loaded: service_loaded(BRIDGE_LABEL),
        config: config_route(settings),
    }
}

#[derive(Debug, Eq, PartialEq)]
struct WatchDecision {
    open_cc_switch: bool,
    bridge: Option<bool>,
}

fn watch_decision(
    cc_ready: bool,
    takeover: bool,
    headroom_ready: bool,
    may_open: bool,
) -> WatchDecision {
    WatchDecision {
        open_cc_switch: takeover && !cc_ready && may_open,
        bridge: if cc_ready && takeover && headroom_ready {
            Some(true)
        } else if cc_ready && !headroom_ready {
            Some(false)
        } else {
            None
        },
    }
}

pub fn watch(settings: &Settings) -> Result<()> {
    let stopped = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGTERM, Arc::clone(&stopped))?;
    signal_hook::flag::register(SIGINT, Arc::clone(&stopped))?;
    let mut last_open: Option<Instant> = None;
    let mut last_error = String::new();

    while !stopped.load(Ordering::Relaxed) {
        let cc_ready = tcp_ready(
            settings.cc_host,
            settings.cc_port,
            Duration::from_millis(400),
        );
        let takeover = cc_takeover_enabled(settings);
        let headroom_ready = headroom_ready(settings);
        let may_open = last_open.is_none_or(|last| last.elapsed() > Duration::from_secs(30));
        let decision = watch_decision(cc_ready, takeover, headroom_ready, may_open);

        if decision.open_cc_switch {
            let _ = Command::new("/usr/bin/open")
                .args(["-gja", "CC Switch"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            last_open = Some(Instant::now());
        }
        let result = decision
            .bridge
            .map(|bridge| reconcile(settings, bridge))
            .transpose();
        match result {
            Ok(_) => last_error.clear(),
            Err(error) => {
                let message = error.to_string();
                if message != last_error {
                    eprintln!("bridge: {message}");
                    last_error = message;
                }
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

pub fn ui(settings: &Settings, no_open: bool) -> Result<String> {
    if !dashboard_ready(settings) {
        start_services(settings)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !dashboard_ready(settings) {
            thread::sleep(Duration::from_millis(200));
        }
        if !dashboard_ready(settings) {
            bail!(
                "Headroom dashboard did not start; see {}",
                settings.state_dir.join("headroom.err.log").display()
            );
        }
    }

    let url = format!("{}/dashboard", settings.headroom_origin());
    if !no_open {
        let status = Command::new("/usr/bin/open")
            .arg(&url)
            .status()
            .context("failed to open browser")?;
        if !status.success() {
            bail!("failed to open browser for {url}");
        }
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use tempfile::TempDir;

    fn settings(root: &Path, port: u16) -> Settings {
        Settings {
            home: root.to_path_buf(),
            config_path: root.join("config.toml"),
            cc_db_path: root.join("cc-switch.db"),
            state_dir: root.join("state"),
            launch_agents_dir: root.join("LaunchAgents"),
            headroom_host: "127.0.0.1",
            headroom_port: port,
            cc_host: "127.0.0.1",
            cc_port: 15721,
        }
    }

    #[test]
    fn reads_cc_switch_takeover_from_sqlite() {
        let temp = TempDir::new().unwrap();
        let settings = settings(temp.path(), 8787);
        let db = Connection::open(&settings.cc_db_path).unwrap();
        db.execute(
            "CREATE TABLE proxy_config (app_type TEXT, enabled INTEGER)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO proxy_config (app_type, enabled) VALUES ('codex', 1)",
            [],
        )
        .unwrap();
        drop(db);
        assert!(cc_takeover_enabled(&settings));
    }

    #[test]
    fn takeover_off_never_opens_cc_switch() {
        assert_eq!(
            watch_decision(false, false, true, true),
            WatchDecision {
                open_cc_switch: false,
                bridge: None,
            }
        );
    }

    #[test]
    fn ui_uses_running_headroom_dashboard_without_launchd() {
        let temp = TempDir::new().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert!(line.starts_with("GET /dashboard "));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        let url = ui(&settings(temp.path(), port), true).unwrap();
        assert_eq!(url, format!("http://127.0.0.1:{port}/dashboard"));
        server.join().unwrap();
    }

    #[test]
    fn bridge_launch_agent_points_at_persisted_settings() {
        let temp = TempDir::new().unwrap();
        let settings = settings(temp.path(), 9898);
        let Value::Dictionary(data) = bridge_plist(&settings, Path::new("/tmp/bridge")).unwrap()
        else {
            panic!("bridge plist was not a dictionary");
        };
        let environment = data
            .get("EnvironmentVariables")
            .and_then(Value::as_dictionary)
            .unwrap();
        assert_eq!(
            environment
                .get("CODEX_HEADROOM_BRIDGE_STATE")
                .and_then(Value::as_string),
            settings.state_dir.to_str()
        );
    }
}
