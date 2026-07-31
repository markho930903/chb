# Codex Headroom Bridge

Routes Codex Desktop through `Headroom -> CC Switch` while leaving provider
identity, credentials, switching, history, and protocol conversion owned by
CC Switch. The bridge is a native macOS Rust binary; Headroom remains managed
by `uv`.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/markho930903/codex-headroom-bridge/main/install.sh | sh
```

The installer supports Apple Silicon and Intel macOS, installs `uv` when
missing, installs Headroom, downloads the matching checksummed bridge binary,
then registers both user LaunchAgents. It does not enable or modify CC Switch
routing settings.

To install from a local checkout instead:

```bash
uv tool install --python 3.13 "headroom-ai[all]"
uv tool uninstall codex-headroom-bridge 2>/dev/null || true
cargo install --path . --locked --force
chb install
```

In CC Switch, enable the local proxy and enable Codex application routing. The
bridge then maintains this route:

```text
Codex Desktop -> 127.0.0.1:8787 Headroom -> 127.0.0.1:15721 CC Switch -> provider
```

While CC Switch routing takeover is disabled, the watcher stays passive: it
does not launch CC Switch or modify Codex configuration, so normal direct
provider switching remains owned by CC Switch.

## Operations

```bash
chb ui
chb ui --no-open
chb doctor
chb status
chb stop
chb start
chb sync
chb rm
```

`ui` starts the installed services when needed, waits for Headroom's local
dashboard, then opens `http://127.0.0.1:8787/dashboard`. The server stays bound
to loopback and the bridge does not enable full-message logging.

`rm` removes both LaunchAgents and restores direct routing to the CC Switch
proxy. The previous `reconcile` and `uninstall` names remain available as
aliases for `sync` and `rm`. Configuration backups remain under
`~/.local/state/codex-headroom-bridge/backups/`.

The installer also keeps `codex-headroom-bridge` as a compatibility link to
`chb`; new commands and LaunchAgents use the short name.

The effective install paths and ports are stored in
`~/.local/state/codex-headroom-bridge/settings.toml`, so LaunchAgents and later
CLI invocations use the same settings.
