# Codex Headroom Bridge

Routes Codex Desktop through `Headroom -> CC Switch` while leaving provider
identity, credentials, switching, history, and protocol conversion owned by
CC Switch.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/markho930903/codex-headroom-bridge/main/install.sh | sh
```

The installer supports macOS, installs `uv` when missing, installs Headroom and
the bridge CLI, then registers both user LaunchAgents. It does not enable or
modify CC Switch routing settings.

To install from a local checkout instead:

```bash
uv tool install --python 3.13 "headroom-ai[all]"
uv tool install .
codex-headroom-bridge install
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
codex-headroom-bridge doctor
codex-headroom-bridge status
codex-headroom-bridge stop
codex-headroom-bridge start
codex-headroom-bridge uninstall
```

`uninstall` removes both LaunchAgents and restores direct routing to the CC
Switch proxy. Configuration backups remain under
`~/.local/state/codex-headroom-bridge/backups/`.
