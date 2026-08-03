use crate::config::{ConfigRoute, RouteKind, config_route, reconcile, snapshot};
use crate::fsutil::atomic_replace_text;
use crate::settings::Settings;
use anyhow::{Context, Result, bail};
use plist::{Dictionary, Value};
use signal_hook::consts::{SIGINT, SIGTERM};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub const PROXY_LABEL: &str = "ai.headroom.codex-ccswitch.proxy";
pub const BRIDGE_LABEL: &str = "ai.headroom.codex-ccswitch.bridge";
pub const WEB_LABEL: &str = "ai.headroom.codex-ccswitch.web";

pub struct RuntimeStatus {
    pub headroom_ready: bool,
    pub proxy_service_loaded: bool,
    pub bridge_service_loaded: bool,
    pub web_service_loaded: bool,
    pub config: ConfigRoute,
}

impl RuntimeStatus {
    pub fn provider_config_ready(&self) -> bool {
        self.config.provider.is_some()
            && !matches!(self.config.route, RouteKind::Missing | RouteKind::Invalid)
    }

    pub fn proxy_enabled(&self) -> bool {
        self.proxy_service_loaded && self.bridge_service_loaded
    }

    pub fn proxy_healthy(&self) -> bool {
        self.headroom_ready && self.proxy_enabled() && self.config.route == RouteKind::Bridged
    }

    pub fn healthy(&self) -> bool {
        self.proxy_healthy() && self.web_service_loaded
    }
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

fn web_ready(settings: &Settings) -> bool {
    http_success(
        settings.web_host,
        settings.web_port,
        "/readyz",
        Duration::from_millis(800),
    )
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

fn web_plist(settings: &Settings, bridge_bin: &Path) -> Result<Value> {
    let mut data = Dictionary::new();
    data.insert("Label".into(), Value::String(WEB_LABEL.into()));
    data.insert(
        "ProgramArguments".into(),
        string_array(&[path_string(bridge_bin)?, "serve".into()]),
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
        Value::String(path_string(&settings.state_dir.join("web.log"))?),
    );
    data.insert(
        "StandardErrorPath".into(),
        Value::String(path_string(&settings.state_dir.join("web.err.log"))?),
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
    let args = ["bootstrap".into(), domain, path];
    for attempt in 0..10 {
        match launchctl(&args, true) {
            Ok(_) => return Ok(()),
            Err(_) if attempt < 9 => thread::sleep(Duration::from_millis(100)),
            Err(error) => return Err(error),
        }
    }
    unreachable!()
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
        (WEB_LABEL, web_plist(settings, bridge_bin)?),
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
            .context("could not restore the selected upstream; Headroom was left running")?;
    }
    bootout(PROXY_LABEL);
    Ok(())
}

pub fn uninstall_services(settings: &Settings) -> Result<()> {
    stop_services(settings)?;
    bootout(WEB_LABEL);
    for label in [WEB_LABEL, BRIDGE_LABEL, PROXY_LABEL] {
        match fs::remove_file(plist_path(settings, label)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("failed to remove LaunchAgent"),
        }
    }
    settings.remove_manifest()?;
    Ok(())
}

pub fn uninstall(settings: &Settings, remove_headroom: bool) -> Result<()> {
    uninstall_services(settings)?;

    if remove_headroom {
        uninstall_headroom(settings)?;
    }

    uninstall_cargo_bridge(settings)?;
    for path in [
        settings.home.join(".local/bin/chb"),
        std::env::current_exe().context("failed to locate the running chb executable")?,
    ] {
        remove_file_if_exists(&path)?;
    }
    remove_dir_if_exists(&settings.state_dir, &settings.home)?;
    Ok(())
}

fn uninstall_cargo_bridge(settings: &Settings) -> Result<()> {
    if !settings.home.join(".cargo/bin/chb").exists() {
        return Ok(());
    }
    let cargo = settings.home.join(".cargo/bin/cargo");
    let status = Command::new(&cargo)
        .args(["uninstall", env!("CARGO_PKG_NAME")])
        .status()
        .with_context(|| format!("failed to run {}", cargo.display()))?;
    if !status.success() {
        bail!("cargo could not uninstall CHB");
    }
    Ok(())
}

fn uninstall_headroom(settings: &Settings) -> Result<()> {
    let tool = settings.home.join(".local/share/uv/tools/headroom-ai");
    if tool.exists() {
        let uv = settings.home.join(".local/bin/uv");
        let status = Command::new(&uv)
            .args(["tool", "uninstall", "headroom-ai"])
            .status()
            .with_context(|| format!("failed to run {}", uv.display()))?;
        if !status.success() {
            bail!("uv could not uninstall Headroom");
        }
    }
    remove_file_if_exists(&settings.home.join(".local/bin/headroom"))?;
    remove_dir_if_exists(&settings.home.join(".headroom"), &settings.home)?;
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn remove_dir_if_exists(path: &Path, home: &Path) -> Result<()> {
    if path == home || !path.starts_with(home) {
        bail!("refusing to remove unsafe directory: {}", path.display());
    }
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

pub fn status(settings: &Settings) -> RuntimeStatus {
    RuntimeStatus {
        headroom_ready: headroom_ready(settings),
        proxy_service_loaded: service_loaded(PROXY_LABEL),
        bridge_service_loaded: service_loaded(BRIDGE_LABEL),
        web_service_loaded: service_loaded(WEB_LABEL),
        config: config_route(settings),
    }
}

pub fn watch(settings: &Settings) -> Result<()> {
    let stopped = stop_signal()?;
    watch_until(settings, &stopped);
    Ok(())
}

fn watch_until(settings: &Settings, stopped: &AtomicBool) {
    let mut last_error = String::new();

    while !stopped.load(Ordering::Relaxed) {
        reconcile_with_log(settings, &mut last_error);
        thread::sleep(Duration::from_millis(500));
    }
}

fn stop_signal() -> Result<Arc<AtomicBool>> {
    let stopped = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGTERM, Arc::clone(&stopped))?;
    signal_hook::flag::register(SIGINT, Arc::clone(&stopped))?;
    Ok(stopped)
}

fn reconcile_with_log(settings: &Settings, last_error: &mut String) {
    match reconcile(settings, headroom_ready(settings)) {
        Ok(_) => last_error.clear(),
        Err(error) => {
            let message = error.to_string();
            if message != *last_error {
                eprintln!("bridge: {message}");
                *last_error = message;
            }
        }
    }
}

pub fn ui(settings: &Settings, no_open: bool) -> Result<String> {
    if !web_ready(settings) {
        if !plist_path(settings, WEB_LABEL).exists() {
            bail!("CHB Web service is not installed; run `chb install`");
        }
        if service_loaded(WEB_LABEL) {
            launchctl(
                &["kickstart".into(), "-k".into(), launch_target(WEB_LABEL)],
                true,
            )?;
        } else {
            bootstrap(settings, WEB_LABEL)?;
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !web_ready(settings) {
            thread::sleep(Duration::from_millis(200));
        }
        if !web_ready(settings) {
            bail!(
                "CHB Web service did not start; see {}",
                settings.state_dir.join("web.err.log").display()
            );
        }
    }

    let url = settings.web_origin();
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

    fn settings(root: &Path, web_port: u16) -> Settings {
        Settings {
            home: root.to_path_buf(),
            config_path: root.join("config.toml"),
            cc_db_path: root.join("cc-switch.db"),
            state_dir: root.join("state"),
            launch_agents_dir: root.join("LaunchAgents"),
            web_host: "127.0.0.1",
            web_port,
            headroom_host: "127.0.0.1",
            headroom_port: 8787,
            cc_host: "127.0.0.1",
            cc_port: 15721,
        }
    }

    #[test]
    fn ui_uses_running_chb_web_service_without_launchd() {
        let temp = TempDir::new().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert!(line.starts_with("GET /readyz "));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        let url = ui(&settings(temp.path(), port), true).unwrap();
        assert_eq!(url, format!("http://127.0.0.1:{port}"));
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

    #[test]
    fn web_launch_agent_runs_the_embedded_server() {
        let temp = TempDir::new().unwrap();
        let settings = settings(temp.path(), 9797);
        let Value::Dictionary(data) = web_plist(&settings, Path::new("/tmp/chb")).unwrap() else {
            panic!("web plist was not a dictionary");
        };
        let arguments = data
            .get("ProgramArguments")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(arguments[0].as_string(), Some("/tmp/chb"));
        assert_eq!(arguments[1].as_string(), Some("serve"));
    }

    #[test]
    fn directory_cleanup_stays_below_home() {
        let temp = TempDir::new().unwrap();
        let owned = temp.path().join("state");
        fs::create_dir(&owned).unwrap();
        remove_dir_if_exists(&owned, temp.path()).unwrap();
        assert!(!owned.exists());
        assert!(remove_dir_if_exists(temp.path(), temp.path()).is_err());
    }

    #[test]
    fn watch_bridges_and_restores_a_loopback_proxy() {
        let temp = TempDir::new().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut settings = settings(temp.path(), 8788);
        settings.headroom_port = port;
        fs::write(
            &settings.config_path,
            "model_provider = \"custom\"\n\n[model_providers.custom]\nbase_url = \"https://provider.example/v1\"\n",
        )
        .unwrap();
        let proxy_stopped = Arc::new(AtomicBool::new(false));
        let proxy_flag = Arc::clone(&proxy_stopped);
        let proxy = thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            while !proxy_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut line = String::new();
                        BufReader::new(stream.try_clone().unwrap())
                            .read_line(&mut line)
                            .unwrap();
                        assert!(line.starts_with("GET /readyz "));
                        stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                            .unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("fake Headroom failed: {error}"),
                }
            }
        });
        let watch_stopped = Arc::new(AtomicBool::new(false));
        let watch_flag = Arc::clone(&watch_stopped);
        let watch_settings = settings.clone();
        let watcher = thread::spawn(move || watch_until(&watch_settings, &watch_flag));

        wait_for_route(&settings, RouteKind::Bridged);
        proxy_stopped.store(true, Ordering::Relaxed);
        proxy.join().unwrap();
        wait_for_route(&settings, RouteKind::Direct);
        watch_stopped.store(true, Ordering::Relaxed);
        watcher.join().unwrap();

        let text = fs::read_to_string(&settings.config_path).unwrap();
        assert!(text.contains("base_url = \"https://provider.example/v1\""));
        assert!(!text.contains("X-Headroom-Base-Url"));
    }

    fn wait_for_route(settings: &Settings, expected: RouteKind) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if config_route(settings).route == expected {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("route did not become {expected:?}");
    }
}
