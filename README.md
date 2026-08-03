# Codex Headroom Bridge

Routes Codex Desktop through `Headroom -> selected provider` while leaving
provider identity, credentials, switching, and history owned by CC Switch. Its
local proxy is optional: when enabled, the route becomes
`Headroom -> CC Switch proxy -> selected provider`. The bridge is a native
macOS Rust binary; Headroom remains managed by `uv`.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/markho930903/chb/main/install.sh | sh
```

The installer supports Apple Silicon and Intel macOS, installs `uv` when
missing, installs Headroom, downloads the matching checksummed bridge binary,
then registers three user LaunchAgents for Headroom, the bridge watcher, and the
CHB Web service. It does not enable or modify CC Switch routing settings.

To install from a local checkout instead:

```bash
uv tool install --python 3.13 "headroom-ai[all]"
cargo install --path . --locked --force
chb install
```

Select providers normally in CC Switch; its local proxy does not need to be
enabled. The bridge maintains this route:

```text
Codex Desktop -> 127.0.0.1:8787 Headroom -> selected provider
```

The watcher captures the upstream URL whenever CC Switch updates the Codex
configuration, keeps Headroom in the request path, and restores that URL when
the Headroom proxy is stopped. If the CC Switch local proxy is enabled, its
loopback URL is preserved as Headroom's upstream instead of competing for the
Codex route. Provider switching remains owned by CC Switch.

## Development

Run the local verification suite from the checkout:

```bash
make dev
```

It runs formatting, Clippy, and the test suite. The watcher smoke test uses only
a temporary configuration and a loopback Headroom substitute, so it does not
modify the local Codex configuration or install LaunchAgents.

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
chb update
chb uninstall
chb uninstall --headroom
```

`ui` opens CHB's own control page at `http://127.0.0.1:8788`. It shows the
effective Codex route, provider, service health, endpoints, and resolved paths.
The page can start or stop the Headroom proxy; stopping the proxy leaves the CHB
Web service online so it can be started again. Both local servers stay bound to
loopback. The `Proxy data` page at `/data` reads Headroom's Codex request, token,
compression, model, provider, WebSocket, latency, and lifetime savings data. It
is read-only and the bridge does not enable full-message logging.

`rm` removes all three LaunchAgents and restores direct routing to the captured
upstream while keeping CHB and its configuration backups installed. `uninstall`
also removes CHB binaries and state; pass `--headroom` to additionally remove
the uv-managed Headroom installation and `~/.headroom` data.

`update` runs the current installer, which upgrades Headroom, installs the
checksummed latest CHB release, and refreshes the user LaunchAgents.

The effective install paths and ports, including the CHB Web port, are stored in
`~/.local/state/codex-headroom-bridge/settings.toml`, so LaunchAgents and later
CLI invocations use the same settings.
