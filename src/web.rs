use crate::macos::{RuntimeStatus, start_services, status, stop_services};
use crate::settings::Settings;
use anyhow::{Context, Result, bail};
use std::fmt::Write as FmtWrite;
use std::io::{BufRead, BufReader, Read, Write as IoWrite};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_HEADROOM_STATS_BYTES: usize = 4 * 1024 * 1024;

pub fn serve(settings: &Settings) -> Result<()> {
    let listener =
        TcpListener::bind((settings.web_host, settings.web_port)).with_context(|| {
            format!(
                "failed to bind CHB Web service at {}",
                settings.web_origin()
            )
        })?;
    let action_lock = Arc::new(Mutex::new(()));

    for connection in listener.incoming() {
        let stream = match connection {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("web: failed to accept connection: {error}");
                continue;
            }
        };
        let settings = settings.clone();
        let action_lock = Arc::clone(&action_lock);
        thread::spawn(move || {
            if let Err(error) = handle_client(stream, &settings, &action_lock) {
                eprintln!("web: {error:#}");
            }
        });
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct Request {
    method: String,
    path: String,
    host: Option<String>,
    action_header: bool,
}

fn read_request(reader: &mut impl BufRead) -> Result<Request> {
    let mut total = 0;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    total += line.len();
    if total > MAX_HEADER_BYTES {
        bail!("request headers are too large");
    }

    let mut parts = line.split_whitespace();
    let method = parts
        .next()
        .context("request method is missing")?
        .to_owned();
    let target = parts.next().context("request target is missing")?;
    let path = target.split('?').next().unwrap_or(target).to_owned();
    let version = parts.next().context("HTTP version is missing")?;
    if parts.next().is_some() || !version.starts_with("HTTP/1.") {
        bail!("invalid request line");
    }

    let mut host = None;
    let mut action_header = false;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            bail!("request headers ended unexpectedly");
        }
        total += line.len();
        if total > MAX_HEADER_BYTES {
            bail!("request headers are too large");
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            bail!("invalid request header");
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("host") {
            host = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("x-chb-action") && value == "1" {
            action_header = true;
        }
    }

    Ok(Request {
        method,
        path,
        host,
        action_header,
    })
}

fn handle_client(
    mut stream: TcpStream,
    settings: &Settings,
    action_lock: &Mutex<()>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let request = {
        let mut reader = BufReader::new(&mut stream);
        match read_request(&mut reader) {
            Ok(request) => request,
            Err(error) => {
                return write_response(
                    &mut stream,
                    "400 Bad Request",
                    "application/json; charset=utf-8",
                    &error_json(&error.to_string()),
                );
            }
        }
    };

    let expected_host = format!("{}:{}", settings.web_host, settings.web_port);
    let localhost = format!("localhost:{}", settings.web_port);
    if !matches!(request.host.as_deref(), Some(host) if host == expected_host || host == localhost)
    {
        return write_response(
            &mut stream,
            "403 Forbidden",
            "application/json; charset=utf-8",
            r#"{"error":"invalid Host header"}"#,
        );
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => write_response(&mut stream, "200 OK", "text/html; charset=utf-8", PAGE),
        ("GET", "/data") => {
            write_response(&mut stream, "200 OK", "text/html; charset=utf-8", DATA_PAGE)
        }
        ("GET", "/app.css") => {
            write_response(&mut stream, "200 OK", "text/css; charset=utf-8", STYLES)
        }
        ("GET", "/app.js") => write_response(
            &mut stream,
            "200 OK",
            "text/javascript; charset=utf-8",
            SCRIPT,
        ),
        ("GET", "/data.js") => write_response(
            &mut stream,
            "200 OK",
            "text/javascript; charset=utf-8",
            DATA_SCRIPT,
        ),
        ("GET", "/readyz") => {
            write_response(&mut stream, "200 OK", "text/plain; charset=utf-8", "ok")
        }
        ("GET", "/api/status") => write_response(
            &mut stream,
            "200 OK",
            "application/json; charset=utf-8",
            &status_json(settings, &status(settings)),
        ),
        ("GET", "/api/headroom/stats") => match fetch_headroom_stats(settings) {
            Ok(body) => write_response(
                &mut stream,
                "200 OK",
                "application/json; charset=utf-8",
                &body,
            ),
            Err(error) => write_response(
                &mut stream,
                "502 Bad Gateway",
                "application/json; charset=utf-8",
                &error_json(&format!("Headroom stats unavailable: {error:#}")),
            ),
        },
        ("POST", "/api/proxy/start" | "/api/proxy/stop") => {
            if !request.action_header {
                return write_response(
                    &mut stream,
                    "403 Forbidden",
                    "application/json; charset=utf-8",
                    r#"{"error":"missing CHB action header"}"#,
                );
            }
            let _guard = action_lock
                .lock()
                .map_err(|_| anyhow::anyhow!("proxy action lock is poisoned"))?;
            let result = if request.path.ends_with("/start") {
                start_services(settings)
            } else {
                stop_services(settings)
            };
            match result {
                Ok(()) => write_response(
                    &mut stream,
                    "200 OK",
                    "application/json; charset=utf-8",
                    &status_json(settings, &status(settings)),
                ),
                Err(error) => write_response(
                    &mut stream,
                    "500 Internal Server Error",
                    "application/json; charset=utf-8",
                    &error_json(&format!("{error:#}")),
                ),
            }
        }
        ("POST", _) => write_response(
            &mut stream,
            "404 Not Found",
            "application/json; charset=utf-8",
            r#"{"error":"not found"}"#,
        ),
        ("GET", _) => write_response(
            &mut stream,
            "404 Not Found",
            "text/plain; charset=utf-8",
            "Not found",
        ),
        _ => write_response(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain; charset=utf-8",
            "Method not allowed",
        ),
    }
}

fn fetch_headroom_stats(settings: &Settings) -> Result<String> {
    let address = (settings.headroom_host, settings.headroom_port)
        .to_socket_addrs()?
        .next()
        .context("Headroom address did not resolve")?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .context("failed to connect to Headroom")?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    write!(
        stream,
        "GET /stats?cached=1 HTTP/1.1\r\nHost: {}:{}\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
        settings.headroom_host, settings.headroom_port
    )?;

    let mut response = Vec::new();
    stream
        .take((MAX_HEADROOM_STATS_BYTES + 1) as u64)
        .read_to_end(&mut response)?;
    decode_headroom_json(&response).map(str::to_owned)
}

fn decode_headroom_json(response: &[u8]) -> Result<&str> {
    if response.len() > MAX_HEADROOM_STATS_BYTES {
        bail!("response exceeded {} bytes", MAX_HEADROOM_STATS_BYTES);
    }
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("Headroom returned an invalid HTTP response")?;
    let headers = std::str::from_utf8(&response[..header_end])?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .context("Headroom response status is invalid")?;
    if !(200..=299).contains(&status) {
        bail!("Headroom returned HTTP {status}");
    }
    let body = std::str::from_utf8(&response[header_end + 4..])?;
    if !body.trim_start().starts_with('{') {
        bail!("Headroom returned a non-object JSON response");
    }
    Ok(body)
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\nCross-Origin-Resource-Policy: same-origin\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body.as_bytes())?;
    Ok(())
}

fn status_json(settings: &Settings, data: &RuntimeStatus) -> String {
    format!(
        concat!(
            "{{",
            "\"version\":{},",
            "\"proxy_enabled\":{},",
            "\"proxy_healthy\":{},",
            "\"headroom_ready\":{},",
            "\"provider_config_ready\":{},",
            "\"proxy_service_loaded\":{},",
            "\"bridge_service_loaded\":{},",
            "\"web_service_loaded\":{},",
            "\"provider\":{},",
            "\"route\":{},",
            "\"upstream\":{},",
            "\"web_origin\":{},",
            "\"headroom_origin\":{},",
            "\"config_path\":{},",
            "\"state_dir\":{}",
            "}}"
        ),
        json_string(env!("CARGO_PKG_VERSION")),
        data.proxy_enabled(),
        data.proxy_healthy(),
        data.headroom_ready,
        data.provider_config_ready(),
        data.proxy_service_loaded,
        data.bridge_service_loaded,
        data.web_service_loaded,
        json_string(data.config.provider.as_deref().unwrap_or("None")),
        json_string(&data.config.route.to_string()),
        json_string(data.config.upstream.as_deref().unwrap_or("None")),
        json_string(&settings.web_origin()),
        json_string(&settings.headroom_origin()),
        json_string(&settings.config_path.display().to_string()),
        json_string(&settings.state_dir.display().to_string()),
    )
}

fn error_json(message: &str) -> String {
    format!(r#"{{"error":{}}}"#, json_string(message))
}

fn json_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                write!(escaped, "\\u{:04x}", u32::from(character)).unwrap();
            }
            character => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

const PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="color-scheme" content="light">
  <title>CHB Control</title>
  <link rel="stylesheet" href="/app.css">
</head>
<body>
  <header class="app-header">
    <div class="shell header-inner">
      <a class="brand" href="/" aria-label="CHB Control home">
        <span class="brand-mark">CHB</span>
        <span>
          <strong>Codex Headroom Bridge</strong>
          <small id="version">Version --</small>
        </span>
      </a>
      <nav class="app-nav" aria-label="CHB pages">
        <a class="active" href="/" aria-current="page">Control</a>
        <a href="/data">Proxy data</a>
      </nav>
      <div class="header-actions">
        <span class="last-check" id="last-check">Checking status</span>
        <a class="button secondary" id="headroom-link" href="#" target="_blank" rel="noreferrer" hidden>Open Headroom</a>
      </div>
    </div>
  </header>

  <main>
    <section class="overview" aria-labelledby="overview-title">
      <div class="shell overview-inner">
        <div>
          <p class="section-label">Local proxy control</p>
          <h1 id="overview-title">Codex routing</h1>
          <p class="overview-copy" id="overall-copy">Reading the active route...</p>
        </div>
        <div class="master-control">
          <div>
            <span class="control-label">Headroom proxy</span>
            <strong id="switch-label">Checking</strong>
          </div>
          <button class="switch" id="proxy-toggle" type="button" role="switch" aria-checked="false" aria-label="Toggle Headroom proxy" disabled>
            <span class="switch-thumb"></span>
          </button>
        </div>
      </div>
    </section>

    <section class="route-band" aria-labelledby="route-title">
      <div class="shell">
        <div class="section-heading">
          <div>
            <p class="section-label">Effective path</p>
            <h2 id="route-title">Request route</h2>
          </div>
          <strong class="route-state" id="route-state">Checking</strong>
        </div>
        <ol class="route-track">
          <li class="route-node" id="route-codex">
            <span>01</span>
            <strong>Codex Desktop</strong>
            <small>Active provider config</small>
          </li>
          <li class="route-node" id="route-headroom">
            <span>02</span>
            <strong>Headroom</strong>
            <small id="headroom-origin">127.0.0.1</small>
          </li>
          <li class="route-node" id="route-provider">
            <span>03</span>
            <strong id="provider">Provider</strong>
            <small id="provider-origin">Selected upstream</small>
          </li>
        </ol>
      </div>
    </section>

    <div class="shell workspace">
      <section class="health" aria-labelledby="health-title">
        <div class="section-heading">
          <div>
            <p class="section-label">Live checks</p>
            <h2 id="health-title">Runtime health</h2>
          </div>
        </div>
        <ul class="status-list">
          <li class="status-row" data-check="web_service_loaded">
            <span class="status-dot" aria-hidden="true"></span>
            <div><strong>CHB Web service</strong><small>ai.headroom.codex-ccswitch.web</small></div>
            <b>Checking</b>
          </li>
          <li class="status-row" data-check="proxy_service_loaded">
            <span class="status-dot" aria-hidden="true"></span>
            <div><strong>Headroom LaunchAgent</strong><small>ai.headroom.codex-ccswitch.proxy</small></div>
            <b>Checking</b>
          </li>
          <li class="status-row" data-check="bridge_service_loaded">
            <span class="status-dot" aria-hidden="true"></span>
            <div><strong>Bridge watcher</strong><small>ai.headroom.codex-ccswitch.bridge</small></div>
            <b>Checking</b>
          </li>
          <li class="status-row" data-check="headroom_ready">
            <span class="status-dot" aria-hidden="true"></span>
            <div><strong>Headroom API</strong><small>HTTP readiness probe</small></div>
            <b>Checking</b>
          </li>
          <li class="status-row" data-check="provider_config_ready">
            <span class="status-dot" aria-hidden="true"></span>
            <div><strong>Provider configuration</strong><small>CC Switch selected provider</small></div>
            <b>Checking</b>
          </li>
        </ul>
      </section>

      <section class="details" aria-labelledby="details-title">
        <div class="section-heading">
          <div>
            <p class="section-label">Resolved settings</p>
            <h2 id="details-title">Configuration</h2>
          </div>
        </div>
        <dl class="detail-list">
          <div><dt>Provider</dt><dd id="detail-provider">--</dd></div>
          <div><dt>Codex route</dt><dd id="detail-route">--</dd></div>
          <div><dt>CHB Web</dt><dd id="web-origin">--</dd></div>
          <div><dt>Headroom</dt><dd id="detail-headroom">--</dd></div>
          <div><dt>Selected upstream</dt><dd id="detail-upstream">--</dd></div>
          <div><dt>Codex config</dt><dd id="config-path">--</dd></div>
          <div><dt>CHB state</dt><dd id="state-dir">--</dd></div>
        </dl>
      </section>
    </div>
  </main>

  <div class="notice" id="notice" role="status" aria-live="polite" hidden></div>
  <script src="/app.js"></script>
</body>
</html>
"##;

const DATA_PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="color-scheme" content="light">
  <title>CHB Proxy Data</title>
  <link rel="stylesheet" href="/app.css">
</head>
<body>
  <header class="app-header">
    <div class="shell header-inner">
      <a class="brand" href="/" aria-label="CHB Control home">
        <span class="brand-mark">CHB</span>
        <span>
          <strong>Codex Headroom Bridge</strong>
          <small>Proxy data</small>
        </span>
      </a>
      <nav class="app-nav" aria-label="CHB pages">
        <a href="/">Control</a>
        <a class="active" href="/data" aria-current="page">Proxy data</a>
      </nav>
      <div class="header-actions">
        <span class="last-check" id="data-last-check">Checking Headroom</span>
      </div>
    </div>
  </header>

  <main>
    <section class="data-hero" aria-labelledby="data-title">
      <div class="shell data-hero-inner">
        <div>
          <p class="section-label">Headroom telemetry</p>
          <h1 id="data-title">Codex proxy data</h1>
          <p class="overview-copy">Live compression, token, model, and request activity</p>
        </div>
        <div class="data-live" id="data-live">
          <span class="data-live-dot" aria-hidden="true"></span>
          <div>
            <small>Headroom session</small>
            <strong id="data-live-label">Connecting</strong>
          </div>
        </div>
      </div>
    </section>

    <section class="metric-band" aria-labelledby="session-title">
      <div class="shell">
        <div class="section-heading metric-heading">
          <div>
            <p class="section-label">Detected client: Codex</p>
            <h2 id="session-title">Current proxy process</h2>
          </div>
          <span class="coverage" id="coverage">Waiting for request logs</span>
        </div>
        <dl class="metric-grid">
          <div><dt>Requests</dt><dd id="metric-requests">0</dd></div>
          <div><dt>Original input</dt><dd id="metric-before">0</dd></div>
          <div><dt>Optimized input</dt><dd id="metric-after">0</dd></div>
          <div><dt>Tokens saved</dt><dd class="positive" id="metric-saved">0</dd></div>
          <div><dt>Input savings</dt><dd class="positive" id="metric-percent">0%</dd></div>
          <div><dt>Output tokens</dt><dd id="metric-output">0</dd></div>
        </dl>
      </div>
    </section>

    <div class="shell data-workspace">
      <section class="data-section" aria-labelledby="traffic-title">
        <div class="section-heading">
          <div>
            <p class="section-label">Traffic shape</p>
            <h2 id="traffic-title">Models and providers</h2>
          </div>
        </div>
        <div class="distribution-grid">
          <div class="distribution-panel">
            <h3>Model requests</h3>
            <div class="bar-list" id="model-bars"></div>
            <p class="empty-inline" id="model-empty">No Codex model traffic yet</p>
          </div>
          <div class="distribution-panel">
            <h3>Provider requests</h3>
            <div class="bar-list" id="provider-bars"></div>
            <p class="empty-inline" id="provider-empty">No Codex provider traffic yet</p>
          </div>
        </div>
      </section>

      <section class="data-section" aria-labelledby="transport-title">
        <div class="section-heading">
          <div>
            <p class="section-label">Codex transport</p>
            <h2 id="transport-title">WebSocket compression</h2>
          </div>
        </div>
        <dl class="transport-grid">
          <div><dt>Frames attempted</dt><dd id="ws-attempted">0</dd></div>
          <div><dt>Frames compressed</dt><dd id="ws-compressed">0</dd></div>
          <div><dt>Frame tokens saved</dt><dd id="ws-saved">0</dd></div>
          <div><dt>Average processing</dt><dd id="ws-latency">0 ms</dd></div>
        </dl>
      </section>

      <section class="data-section" aria-labelledby="requests-title">
        <div class="section-heading">
          <div>
            <p class="section-label">Latest detected traffic</p>
            <h2 id="requests-title">Recent Codex requests</h2>
          </div>
          <span class="coverage" id="recent-count">0 requests</span>
        </div>
        <div class="table-wrap">
          <table class="request-table" hidden>
            <thead>
              <tr>
                <th>Time</th>
                <th>Model</th>
                <th class="numeric">Original</th>
                <th class="numeric">Optimized</th>
                <th class="numeric">Saved</th>
                <th class="numeric">Latency</th>
                <th>Status</th>
              </tr>
            </thead>
            <tbody id="request-rows"></tbody>
          </table>
          <div class="table-empty" id="requests-empty">No Codex requests recorded in this Headroom process</div>
        </div>
      </section>

      <section class="data-section lifetime-section" aria-labelledby="lifetime-title">
        <div class="section-heading">
          <div>
            <p class="section-label">Persisted by Headroom</p>
            <h2 id="lifetime-title">Proxy lifetime</h2>
          </div>
          <span class="coverage" id="lifetime-activity">No saved activity</span>
        </div>
        <dl class="lifetime-grid">
          <div><dt>Requests</dt><dd id="lifetime-requests">0</dd></div>
          <div><dt>Tokens saved</dt><dd id="lifetime-saved">0</dd></div>
          <div><dt>Compression savings</dt><dd id="lifetime-compression-cost">$0.00</dd></div>
          <div><dt>Cache savings</dt><dd id="lifetime-cache-cost">$0.00</dd></div>
        </dl>
      </section>
    </div>
  </main>

  <div class="notice" id="data-notice" role="status" aria-live="polite" hidden></div>
  <script src="/data.js"></script>
</body>
</html>
"##;

const STYLES: &str = r#":root {
  color: #17211e;
  background: #f5f7f6;
  font-family: Inter, ui-sans-serif, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
  font-synthesis: none;
  letter-spacing: 0;
  --ink: #17211e;
  --muted: #66716d;
  --line: #d9dfdc;
  --surface: #ffffff;
  --accent: #087f5b;
  --accent-dark: #18332d;
  --danger: #c63c55;
  --warning: #ad6800;
}

* { box-sizing: border-box; }

body {
  margin: 0;
  min-width: 320px;
  min-height: 100vh;
  background: #f5f7f6;
}

button, a { font: inherit; letter-spacing: 0; }
button { cursor: pointer; }
button:disabled { cursor: wait; }

.shell {
  width: min(1120px, calc(100% - 48px));
  margin: 0 auto;
}

.app-header {
  height: 76px;
  background: var(--surface);
  border-bottom: 1px solid var(--line);
}

.header-inner {
  height: 100%;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 24px;
}

.brand {
  display: inline-flex;
  align-items: center;
  gap: 12px;
  color: var(--ink);
  text-decoration: none;
}

.brand-mark {
  width: 40px;
  height: 40px;
  display: grid;
  place-items: center;
  border-radius: 6px;
  background: var(--accent-dark);
  color: white;
  font-size: 12px;
  font-weight: 800;
}

.brand strong, .brand small { display: block; }
.brand strong { font-size: 14px; line-height: 1.35; }
.brand small { margin-top: 2px; color: var(--muted); font-size: 12px; line-height: 1.3; }

.app-nav {
  align-self: stretch;
  display: flex;
  align-items: stretch;
  gap: 22px;
}

.app-nav a {
  position: relative;
  display: flex;
  align-items: center;
  color: var(--muted);
  font-size: 13px;
  line-height: 1.4;
  font-weight: 700;
  text-decoration: none;
}

.app-nav a:hover { color: var(--ink); }
.app-nav a.active { color: var(--ink); }
.app-nav a.active::after {
  content: "";
  position: absolute;
  right: 0;
  bottom: -1px;
  left: 0;
  height: 2px;
  background: var(--accent);
}

.header-actions {
  display: flex;
  align-items: center;
  gap: 16px;
}

.last-check { color: var(--muted); font-size: 12px; }

.button {
  min-height: 36px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  padding: 0 14px;
  border-radius: 6px;
  font-size: 13px;
  font-weight: 700;
  text-decoration: none;
}

.button.secondary { color: var(--ink); border: 1px solid var(--line); background: white; }
.button.secondary:hover { border-color: #9aa6a1; background: #f7f9f8; }

.overview { background: var(--accent-dark); color: white; }

.overview-inner {
  min-height: 202px;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 48px;
}

.section-label {
  margin: 0 0 8px;
  color: var(--accent);
  font-size: 11px;
  line-height: 1.4;
  font-weight: 800;
  text-transform: uppercase;
}

.overview .section-label { color: #80d7b8; }
h1, h2, p { letter-spacing: 0; }
h1 { margin: 0; font-size: 38px; line-height: 1.12; font-weight: 720; }
h2 { margin: 0; font-size: 20px; line-height: 1.25; font-weight: 720; }

.overview-copy {
  margin: 14px 0 0;
  color: #c5d5cf;
  font-size: 15px;
  line-height: 1.5;
}

.master-control {
  min-width: 292px;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 32px;
  padding: 22px 24px;
  border: 1px solid #49645b;
  border-radius: 8px;
  background: #213f37;
}

.control-label { display: block; color: #aebfba; font-size: 12px; line-height: 1.4; }
.master-control strong { display: block; margin-top: 3px; font-size: 16px; line-height: 1.35; }

.switch {
  position: relative;
  flex: 0 0 auto;
  width: 54px;
  height: 30px;
  padding: 0;
  border: 0;
  border-radius: 15px;
  background: #74827d;
  transition: background 160ms ease;
}

.switch-thumb {
  position: absolute;
  top: 4px;
  left: 4px;
  width: 22px;
  height: 22px;
  border-radius: 50%;
  background: white;
  box-shadow: 0 1px 3px #08110e80;
  transition: transform 160ms ease;
}

.switch[aria-checked="true"] { background: #24b47e; }
.switch[aria-checked="true"] .switch-thumb { transform: translateX(24px); }
.switch:disabled { opacity: .62; }
.switch:focus-visible, .button:focus-visible { outline: 3px solid #58a6ff; outline-offset: 3px; }

.route-band { background: var(--surface); border-bottom: 1px solid var(--line); }
.route-band .shell { padding: 34px 0 38px; }

.section-heading {
  min-height: 45px;
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 24px;
}

.route-state {
  color: var(--muted);
  font-size: 12px;
  line-height: 1.4;
  font-weight: 750;
}

.route-state.good { color: var(--accent); }
.route-state.warn { color: var(--warning); }
.route-state.off { color: var(--danger); }

.route-track {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 0;
  margin: 28px 0 0;
  padding: 0;
  list-style: none;
}

.route-node {
  position: relative;
  min-width: 0;
  padding: 0 26px 0 0;
}

.route-node:not(:last-child)::after {
  content: "";
  position: absolute;
  top: 13px;
  left: 38px;
  right: 10px;
  height: 1px;
  background: var(--line);
}

.route-node > span {
  position: relative;
  z-index: 1;
  width: 28px;
  height: 28px;
  display: grid;
  place-items: center;
  margin-bottom: 12px;
  border: 1px solid var(--line);
  border-radius: 50%;
  background: white;
  color: var(--muted);
  font-size: 10px;
  font-weight: 800;
}

.route-node.active > span { border-color: var(--accent); background: var(--accent); color: white; }
.route-node strong, .route-node small { display: block; overflow-wrap: anywhere; }
.route-node strong { font-size: 14px; line-height: 1.4; }
.route-node small { margin-top: 3px; color: var(--muted); font: 12px/1.45 ui-monospace, SFMono-Regular, Menlo, monospace; }

.workspace {
  display: grid;
  grid-template-columns: minmax(0, 1.35fr) minmax(300px, .8fr);
  gap: 56px;
  padding-top: 42px;
  padding-bottom: 64px;
}

.details { border-left: 1px solid var(--line); padding-left: 56px; }
.status-list { margin: 21px 0 0; padding: 0; list-style: none; border-top: 1px solid var(--line); }

.status-row {
  min-height: 66px;
  display: grid;
  grid-template-columns: 10px minmax(0, 1fr) auto;
  align-items: center;
  gap: 14px;
  border-bottom: 1px solid var(--line);
}

.status-dot { width: 8px; height: 8px; border-radius: 50%; background: #aeb7b3; }
.status-row.ok .status-dot { background: var(--accent); }
.status-row.fail .status-dot { background: var(--danger); }
.status-row strong, .status-row small { display: block; }
.status-row strong { font-size: 13px; line-height: 1.4; }
.status-row small { margin-top: 2px; color: var(--muted); font-size: 11px; line-height: 1.4; overflow-wrap: anywhere; }
.status-row b { color: var(--muted); font-size: 11px; line-height: 1.4; font-weight: 750; }
.status-row.ok b { color: var(--accent); }
.status-row.fail b { color: var(--danger); }

.detail-list { margin: 21px 0 0; border-top: 1px solid var(--line); }
.detail-list > div { padding: 14px 0; border-bottom: 1px solid var(--line); }
.detail-list dt { color: var(--muted); font-size: 11px; line-height: 1.35; }
.detail-list dd { margin: 5px 0 0; color: var(--ink); font: 12px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace; overflow-wrap: anywhere; }

.data-hero { background: var(--accent-dark); color: white; }
.data-hero .section-label { color: #80d7b8; }

.data-hero-inner {
  min-height: 190px;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 48px;
}

.data-live {
  min-width: 250px;
  display: flex;
  align-items: center;
  gap: 14px;
  padding: 18px 20px;
  border: 1px solid #49645b;
  border-radius: 8px;
  background: #213f37;
}

.data-live-dot { flex: 0 0 auto; width: 10px; height: 10px; border-radius: 50%; background: #aeb7b3; }
.data-live.live .data-live-dot { background: #55d6a4; box-shadow: 0 0 0 5px #55d6a426; }
.data-live.error .data-live-dot { background: #f17288; }
.data-live small, .data-live strong { display: block; }
.data-live small { color: #aebfba; font-size: 11px; line-height: 1.4; }
.data-live strong { margin-top: 3px; font-size: 14px; line-height: 1.4; }

.metric-band { background: white; border-bottom: 1px solid var(--line); }
.metric-heading { padding-top: 30px; }
.coverage { color: var(--muted); font-size: 11px; line-height: 1.4; text-align: right; }

.metric-grid {
  display: grid;
  grid-template-columns: repeat(6, minmax(0, 1fr));
  margin: 25px 0 0;
  padding: 0 0 34px;
}

.metric-grid > div { min-width: 0; padding: 0 18px; border-left: 1px solid var(--line); }
.metric-grid > div:first-child { padding-left: 0; border-left: 0; }
.metric-grid dt, .transport-grid dt, .lifetime-grid dt { color: var(--muted); font-size: 11px; line-height: 1.4; }
.metric-grid dd { margin: 8px 0 0; font-size: 26px; line-height: 1.15; font-weight: 680; overflow-wrap: anywhere; }
.metric-grid dd.positive { color: var(--accent); }

.data-workspace { padding-top: 8px; padding-bottom: 64px; }
.data-section { padding: 38px 0 42px; border-bottom: 1px solid var(--line); }
.data-section:last-child { border-bottom: 0; }

.distribution-grid {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  margin-top: 24px;
}

.distribution-panel { min-width: 0; padding-right: 42px; }
.distribution-panel + .distribution-panel { padding-right: 0; padding-left: 42px; border-left: 1px solid var(--line); }
.distribution-panel h3 { margin: 0 0 18px; font-size: 13px; line-height: 1.4; }
.bar-list { display: grid; gap: 15px; }

.bar-row { display: grid; grid-template-columns: minmax(90px, 1fr) minmax(120px, 2fr) auto; align-items: center; gap: 14px; }
.bar-label { min-width: 0; font: 12px/1.4 ui-monospace, SFMono-Regular, Menlo, monospace; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.bar-track { height: 6px; overflow: hidden; border-radius: 3px; background: #e2e7e5; }
.bar-fill { height: 100%; border-radius: 3px; background: var(--accent); }
.bar-value { min-width: 28px; color: var(--muted); font: 11px/1.4 ui-monospace, SFMono-Regular, Menlo, monospace; text-align: right; }
.empty-inline { margin: 0; color: var(--muted); font-size: 12px; line-height: 1.5; }

.transport-grid, .lifetime-grid {
  display: grid;
  grid-template-columns: repeat(4, minmax(0, 1fr));
  margin: 24px 0 0;
  padding: 0;
}

.transport-grid > div, .lifetime-grid > div { min-width: 0; padding-left: 24px; border-left: 1px solid var(--line); }
.transport-grid > div:first-child, .lifetime-grid > div:first-child { padding-left: 0; border-left: 0; }
.transport-grid dd, .lifetime-grid dd { margin: 7px 0 0; font: 18px/1.3 ui-monospace, SFMono-Regular, Menlo, monospace; overflow-wrap: anywhere; }

.table-wrap { margin-top: 23px; overflow-x: auto; border-top: 1px solid var(--line); }
.request-table { width: 100%; min-width: 820px; border-collapse: collapse; font-size: 12px; }
.request-table th { height: 42px; color: var(--muted); font-size: 10px; line-height: 1.3; font-weight: 750; text-align: left; text-transform: uppercase; }
.request-table th.numeric, .request-table td.numeric { text-align: right; }
.request-table td { height: 58px; padding: 8px 18px 8px 0; border-top: 1px solid var(--line); vertical-align: middle; }
.request-table td:last-child, .request-table th:last-child { padding-right: 0; }
.request-table .mono { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
.request-table .saved { color: var(--accent); font-weight: 750; }
.request-status { color: var(--accent); font-size: 11px; font-weight: 750; }
.request-status.error { color: var(--danger); }
.table-empty { padding: 34px 0; color: var(--muted); font-size: 12px; line-height: 1.5; text-align: center; }

.lifetime-section { background: white; margin-right: -24px; margin-left: -24px; padding-right: 24px; padding-left: 24px; }

.notice {
  position: fixed;
  right: 24px;
  bottom: 24px;
  max-width: min(390px, calc(100% - 48px));
  padding: 13px 16px;
  border-radius: 6px;
  background: var(--accent-dark);
  color: white;
  box-shadow: 0 8px 24px #101b1740;
  font-size: 13px;
  line-height: 1.45;
}

.notice.error { background: #8f263a; }

@media (max-width: 760px) {
  .shell { width: min(100% - 32px, 560px); }
  .app-header { height: auto; min-height: 72px; }
  .header-inner { flex-wrap: wrap; padding: 12px 0 0; gap: 10px 16px; }
  .app-nav { order: 3; flex: 1 0 100%; height: 38px; gap: 24px; }
  .last-check { display: none; }
  .overview-inner { min-height: 255px; align-items: stretch; flex-direction: column; justify-content: center; gap: 28px; padding: 32px 0; }
  h1 { font-size: 32px; }
  .master-control { min-width: 0; width: 100%; }
  .route-track { grid-template-columns: 1fr; gap: 20px; }
  .route-node { min-height: 58px; padding-left: 44px; }
  .route-node > span { position: absolute; top: 0; left: 0; }
  .route-node:not(:last-child)::after { top: 28px; bottom: -20px; left: 14px; right: auto; width: 1px; height: auto; }
  .workspace { grid-template-columns: 1fr; gap: 42px; padding-top: 36px; }
  .details { border-left: 0; border-top: 1px solid var(--line); padding: 40px 0 0; }
  .data-hero-inner { min-height: 250px; align-items: stretch; flex-direction: column; justify-content: center; gap: 28px; padding: 32px 0; }
  .data-live { min-width: 0; width: 100%; }
  .metric-heading { padding-top: 26px; }
  .metric-grid { grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 22px 0; }
  .metric-grid > div { padding: 0 0 0 18px; }
  .metric-grid > div:nth-child(odd) { padding-left: 0; border-left: 0; }
  .distribution-grid { grid-template-columns: 1fr; gap: 30px; }
  .distribution-panel { padding-right: 0; }
  .distribution-panel + .distribution-panel { padding: 30px 0 0; border-top: 1px solid var(--line); border-left: 0; }
  .bar-row { grid-template-columns: minmax(90px, 1.25fr) minmax(90px, 1.5fr) auto; gap: 10px; }
  .transport-grid, .lifetime-grid { grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 24px 0; }
  .transport-grid > div, .lifetime-grid > div { padding-left: 18px; }
  .transport-grid > div:nth-child(odd), .lifetime-grid > div:nth-child(odd) { padding-left: 0; border-left: 0; }
  .lifetime-section { margin-right: -16px; margin-left: -16px; padding-right: 16px; padding-left: 16px; }
  .notice { right: 16px; bottom: 16px; }
}

@media (prefers-reduced-motion: reduce) {
  *, *::before, *::after { scroll-behavior: auto !important; transition-duration: 0s !important; }
}
"#;

const SCRIPT: &str = r#"const toggle = document.querySelector('#proxy-toggle');
const notice = document.querySelector('#notice');
let current = null;
let refreshing = false;
let noticeTimer = null;

const text = (id, value) => { document.querySelector(id).textContent = value; };

function showNotice(message, error = false) {
  clearTimeout(noticeTimer);
  notice.textContent = message;
  notice.classList.toggle('error', error);
  notice.hidden = false;
  noticeTimer = setTimeout(() => { notice.hidden = true; }, 5000);
}

function setCheck(name, ok, runningLabels = false) {
  const row = document.querySelector(`[data-check="${name}"]`);
  row.classList.toggle('ok', ok);
  row.classList.toggle('fail', !ok);
  row.querySelector('b').textContent = runningLabels
    ? (ok ? 'Running' : 'Stopped')
    : (ok ? 'Ready' : 'Unavailable');
}

function render(data) {
  current = data;
  text('#version', `Version ${data.version}`);
  text('#provider', data.provider === 'None' ? 'No provider' : data.provider);
  text('#detail-provider', data.provider);
  text('#detail-route', data.route);
  text('#web-origin', data.web_origin);
  text('#headroom-origin', data.headroom_origin);
  text('#detail-headroom', data.headroom_origin);
  text('#provider-origin', data.upstream);
  text('#detail-upstream', data.upstream);
  text('#config-path', data.config_path);
  text('#state-dir', data.state_dir);

  toggle.setAttribute('aria-checked', String(data.proxy_enabled));
  toggle.setAttribute('aria-label', data.proxy_enabled ? 'Turn off Headroom proxy' : 'Turn on Headroom proxy');
  text('#switch-label', data.proxy_enabled ? 'On' : 'Off');

  const overall = data.proxy_healthy
    ? 'Proxy on / Codex is routed through Headroom'
    : data.proxy_enabled
      ? 'Proxy on / route needs attention'
      : 'Proxy off / dashboard remains available';
  text('#overall-copy', overall);

  const routeState = document.querySelector('#route-state');
  routeState.className = `route-state ${data.proxy_healthy ? 'good' : data.proxy_enabled ? 'warn' : 'off'}`;
  routeState.textContent = data.proxy_healthy ? 'Route healthy' : data.proxy_enabled ? 'Attention required' : 'Proxy disabled';

  document.querySelector('#route-codex').classList.toggle('active', data.proxy_enabled);
  document.querySelector('#route-headroom').classList.toggle('active', data.headroom_ready);
  document.querySelector('#route-provider').classList.toggle('active', data.proxy_healthy);

  setCheck('web_service_loaded', data.web_service_loaded, true);
  setCheck('proxy_service_loaded', data.proxy_service_loaded, true);
  setCheck('bridge_service_loaded', data.bridge_service_loaded, true);
  setCheck('headroom_ready', data.headroom_ready);
  setCheck('provider_config_ready', data.provider_config_ready);

  const headroomLink = document.querySelector('#headroom-link');
  headroomLink.href = `${data.headroom_origin}/dashboard`;
  headroomLink.hidden = !data.headroom_ready;
  text('#last-check', `Checked ${new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' })}`);
}

async function refresh() {
  if (refreshing) return;
  refreshing = true;
  try {
    const response = await fetch('/api/status', { cache: 'no-store' });
    if (!response.ok) throw new Error(`Status request failed (${response.status})`);
    render(await response.json());
    toggle.disabled = false;
  } catch (error) {
    toggle.disabled = true;
    text('#last-check', 'Status unavailable');
    showNotice(error.message, true);
  } finally {
    refreshing = false;
  }
}

toggle.addEventListener('click', async () => {
  if (!current) return;
  const enable = !current.proxy_enabled;
  toggle.disabled = true;
  toggle.setAttribute('aria-busy', 'true');
  text('#switch-label', enable ? 'Starting' : 'Stopping');
  try {
    const response = await fetch(`/api/proxy/${enable ? 'start' : 'stop'}`, {
      method: 'POST',
      headers: { 'X-CHB-Action': '1' }
    });
    const data = await response.json();
    if (!response.ok) throw new Error(data.error || `Proxy action failed (${response.status})`);
    render(data);
    showNotice(enable ? 'Headroom proxy started' : 'Headroom proxy stopped');
  } catch (error) {
    showNotice(error.message, true);
    await refresh();
  } finally {
    toggle.disabled = false;
    toggle.removeAttribute('aria-busy');
  }
});

document.addEventListener('visibilitychange', () => {
  if (!document.hidden) refresh();
});

refresh();
setInterval(() => { if (!document.hidden) refresh(); }, 3000);
"#;

const DATA_SCRIPT: &str = r#"const notice = document.querySelector('#data-notice');
const live = document.querySelector('#data-live');
let refreshing = false;
let noticeTimer = null;

const integer = new Intl.NumberFormat('en-US', { maximumFractionDigits: 0 });
const money = new Intl.NumberFormat('en-US', { style: 'currency', currency: 'USD' });
const object = (value) => value && typeof value === 'object' && !Array.isArray(value) ? value : {};
const number = (value) => Number.isFinite(Number(value)) ? Number(value) : 0;
const text = (selector, value) => { document.querySelector(selector).textContent = value; };
const formatNumber = (value) => integer.format(number(value));
const formatPercent = (value) => `${number(value).toFixed(1)}%`;

function showNotice(message) {
  clearTimeout(noticeTimer);
  notice.textContent = message;
  notice.classList.add('error');
  notice.hidden = false;
  noticeTimer = setTimeout(() => { notice.hidden = true; }, 5000);
}

function renderBars(containerSelector, emptySelector, values) {
  const container = document.querySelector(containerSelector);
  const empty = document.querySelector(emptySelector);
  const entries = Object.entries(object(values))
    .map(([label, count]) => [label, number(count)])
    .filter(([, count]) => count > 0)
    .sort((left, right) => right[1] - left[1]);
  const maximum = Math.max(0, ...entries.map(([, count]) => count));

  container.replaceChildren();
  for (const [label, count] of entries) {
    const row = document.createElement('div');
    row.className = 'bar-row';

    const name = document.createElement('span');
    name.className = 'bar-label';
    name.title = label;
    name.textContent = label;

    const track = document.createElement('span');
    track.className = 'bar-track';
    track.setAttribute('aria-hidden', 'true');
    const fill = document.createElement('span');
    fill.className = 'bar-fill';
    fill.style.width = `${(count / maximum) * 100}%`;
    track.append(fill);

    const value = document.createElement('span');
    value.className = 'bar-value';
    value.textContent = formatNumber(count);
    row.append(name, track, value);
    container.append(row);
  }
  empty.hidden = entries.length > 0;
}

function tableCell(value, className = '') {
  const cell = document.createElement('td');
  cell.textContent = value;
  cell.className = className;
  return cell;
}

function renderRecent(data) {
  const logs = Array.isArray(data.request_logs) ? data.request_logs : [];
  const logById = new Map();
  for (const log of logs) {
    if (object(log.tags).client === 'codex' && log.request_id) {
      logById.set(log.request_id, log);
    }
  }
  const requests = (Array.isArray(data.recent_requests) ? data.recent_requests : [])
    .filter((request) => logById.has(request.request_id));
  const rows = document.querySelector('#request-rows');
  rows.replaceChildren();

  for (const request of requests) {
    const timestamp = new Date(request.timestamp);
    const time = Number.isNaN(timestamp.getTime())
      ? '--'
      : timestamp.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
    const log = logById.get(request.request_id);
    const failed = Boolean(log.error);
    const status = failed ? 'Failed' : request.has_exact_tokens ? 'Complete' : 'Partial';
    const statusCell = tableCell(status);
    statusCell.className = `request-status${failed ? ' error' : ''}`;

    const row = document.createElement('tr');
    row.append(
      tableCell(time, 'mono'),
      tableCell(String(request.model || 'Unknown'), 'mono'),
      tableCell(formatNumber(request.input_tokens_original), 'numeric mono'),
      tableCell(formatNumber(request.input_tokens_optimized), 'numeric mono'),
      tableCell(formatNumber(request.tokens_saved), 'numeric mono saved'),
      tableCell(`${formatNumber(request.total_latency_ms || request.optimization_latency_ms)} ms`, 'numeric mono'),
      statusCell
    );
    rows.append(row);
  }

  text('#recent-count', `${formatNumber(requests.length)} ${requests.length === 1 ? 'request' : 'requests'}`);
  document.querySelector('#requests-empty').hidden = requests.length > 0;
  document.querySelector('.request-table').hidden = requests.length === 0;
}

function render(data) {
  const usage = object(data.agent_usage);
  const agent = (Array.isArray(usage.agents) ? usage.agents : [])
    .find((item) => item.agent === 'codex');
  const codex = object(agent);
  const coverage = object(usage.coverage);

  text('#metric-requests', formatNumber(codex.requests));
  text('#metric-before', formatNumber(codex.before_tokens));
  text('#metric-after', formatNumber(codex.after_tokens));
  text('#metric-saved', formatNumber(codex.tokens_saved));
  text('#metric-percent', formatPercent(codex.savings_percent));
  text('#metric-output', formatNumber(codex.output_tokens));
  text('#coverage', agent
    ? (codex.has_exact_tokens ? 'Exact request token data' : 'Request totals detected')
    : (number(coverage.logged_requests) > 0 ? 'No Codex traffic in log window' : 'Waiting for request logs'));
  renderBars('#model-bars', '#model-empty', codex.models);
  renderBars('#provider-bars', '#provider-empty', codex.providers);

  const websocket = object(data.codex_ws);
  text('#ws-attempted', formatNumber(websocket.frames_attempted_total));
  text('#ws-compressed', formatNumber(websocket.frames_compressed_total));
  text('#ws-saved', formatNumber(websocket.frame_tokens_saved_sum));
  text('#ws-latency', `${number(object(websocket.frame_elapsed_ms).average).toFixed(1)} ms`);

  const lifetime = object(object(data.persistent_savings).lifetime);
  text('#lifetime-requests', formatNumber(lifetime.requests));
  text('#lifetime-saved', formatNumber(lifetime.tokens_saved));
  text('#lifetime-compression-cost', money.format(number(lifetime.compression_savings_usd)));
  text('#lifetime-cache-cost', money.format(number(lifetime.cache_savings_usd)));
  text('#lifetime-activity', number(lifetime.requests) > 0
    ? `${formatNumber(lifetime.requests)} recorded requests`
    : 'No saved activity');

  renderRecent(data);
  live.className = 'data-live live';
  text('#data-live-label', 'Connected');
  text('#data-last-check', `Updated ${new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' })}`);
  notice.hidden = true;
}

async function refresh() {
  if (refreshing) return;
  refreshing = true;
  try {
    const response = await fetch('/api/headroom/stats', { cache: 'no-store' });
    const data = await response.json();
    if (!response.ok) throw new Error(data.error || `Stats request failed (${response.status})`);
    render(data);
  } catch (error) {
    live.className = 'data-live error';
    text('#data-live-label', 'Unavailable');
    text('#data-last-check', 'Stats unavailable');
    showNotice(error.message);
  } finally {
    refreshing = false;
  }
}

document.addEventListener('visibilitychange', () => {
  if (!document.hidden) refresh();
});

refresh();
setInterval(() => { if (!document.hidden) refresh(); }, 5000);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_local_action_request() {
        let mut input = Cursor::new(
            b"POST /api/proxy/stop?now=1 HTTP/1.1\r\nHost: 127.0.0.1:8788\r\nX-CHB-Action: 1\r\n\r\n",
        );
        assert_eq!(
            read_request(&mut input).unwrap(),
            Request {
                method: "POST".into(),
                path: "/api/proxy/stop".into(),
                host: Some("127.0.0.1:8788".into()),
                action_header: true,
            }
        );
    }

    #[test]
    fn json_strings_escape_control_characters() {
        assert_eq!(json_string("a\"b\\c\n"), r#""a\"b\\c\n""#);
    }

    #[test]
    fn accepts_successful_headroom_json_response() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"requests\":1}";
        assert_eq!(decode_headroom_json(response).unwrap(), r#"{"requests":1}"#);
    }

    #[test]
    fn rejects_unsuccessful_headroom_response() {
        let response = b"HTTP/1.1 503 Service Unavailable\r\n\r\n{\"error\":\"busy\"}";
        assert!(
            decode_headroom_json(response)
                .unwrap_err()
                .to_string()
                .contains("Headroom returned HTTP 503")
        );
    }

    #[test]
    fn rejects_non_object_headroom_response() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n[]";
        assert!(
            decode_headroom_json(response)
                .unwrap_err()
                .to_string()
                .contains("non-object JSON response")
        );
    }

    #[test]
    fn dashboard_exposes_real_controls_and_status_route() {
        assert!(PAGE.contains("id=\"proxy-toggle\""));
        assert!(SCRIPT.contains("/api/proxy/${enable ? 'start' : 'stop'}"));
        assert!(SCRIPT.contains("/api/status"));
    }

    #[test]
    fn data_page_exposes_codex_stats_without_html_injection() {
        assert!(DATA_PAGE.contains("id=\"metric-requests\""));
        assert!(DATA_PAGE.contains("id=\"request-rows\""));
        assert!(DATA_SCRIPT.contains("/api/headroom/stats"));
        assert!(DATA_SCRIPT.contains("item.agent === 'codex'"));
        assert!(DATA_SCRIPT.contains("object(log.tags).client === 'codex'"));
        assert!(DATA_SCRIPT.contains("textContent"));
        assert!(!DATA_SCRIPT.contains("innerHTML"));
    }
}
