from __future__ import annotations

import argparse
import contextlib
import fcntl
import http.client
import json
import os
import plistlib
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any

import tomlkit

BRIDGE_HEADER = "X-Headroom-Base-Url"
PROXY_LABEL = "ai.headroom.codex-ccswitch.proxy"
BRIDGE_LABEL = "ai.headroom.codex-ccswitch.bridge"


@dataclass(frozen=True)
class Settings:
    home: Path
    config_path: Path
    cc_db_path: Path
    state_dir: Path
    launch_agents_dir: Path
    headroom_host: str = "127.0.0.1"
    headroom_port: int = 8787
    cc_host: str = "127.0.0.1"
    cc_port: int = 15721

    @property
    def headroom_base(self) -> str:
        return f"http://{self.headroom_host}:{self.headroom_port}/v1"

    @property
    def cc_base(self) -> str:
        return f"http://{self.cc_host}:{self.cc_port}/v1"

    @property
    def cc_origin(self) -> str:
        return f"http://{self.cc_host}:{self.cc_port}"


def default_settings(args: argparse.Namespace | None = None) -> Settings:
    home = Path(os.environ.get("CODEX_HEADROOM_BRIDGE_HOME", Path.home())).expanduser()
    config = Path(
        getattr(args, "config", None)
        or os.environ.get("CODEX_HEADROOM_BRIDGE_CONFIG", home / ".codex/config.toml")
    ).expanduser()
    cc_db = Path(
        getattr(args, "cc_db", None)
        or os.environ.get("CODEX_HEADROOM_BRIDGE_CC_DB", home / ".cc-switch/cc-switch.db")
    ).expanduser()
    state = Path(
        os.environ.get(
            "CODEX_HEADROOM_BRIDGE_STATE",
            home / ".local/state/codex-headroom-bridge",
        )
    ).expanduser()
    return Settings(
        home=home,
        config_path=config,
        cc_db_path=cc_db,
        state_dir=state,
        launch_agents_dir=home / "Library/LaunchAgents",
        headroom_port=int(getattr(args, "headroom_port", 8787)),
        cc_port=int(getattr(args, "cc_port", 15721)),
    )


def tcp_ready(host: str, port: int, timeout: float = 0.4) -> bool:
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return True
    except OSError:
        return False


def headroom_ready(settings: Settings) -> bool:
    try:
        conn = http.client.HTTPConnection(
            settings.headroom_host, settings.headroom_port, timeout=0.8
        )
        conn.request("GET", "/readyz")
        response = conn.getresponse()
        response.read()
        return 200 <= response.status < 300
    except OSError:
        return False
    finally:
        with contextlib.suppress(Exception):
            conn.close()  # type: ignore[possibly-undefined]


def cc_takeover_enabled(settings: Settings) -> bool:
    if not settings.cc_db_path.exists():
        return False
    try:
        uri = f"file:{settings.cc_db_path}?mode=ro"
        with sqlite3.connect(uri, uri=True, timeout=0.5) as db:
            row = db.execute(
                "SELECT enabled FROM proxy_config WHERE app_type = 'codex'"
            ).fetchone()
        return bool(row and row[0])
    except sqlite3.Error:
        return False


def _header_key(headers: Any) -> str | None:
    if not hasattr(headers, "keys"):
        return None
    return next(
        (str(key) for key in headers.keys() if str(key).casefold() == BRIDGE_HEADER.casefold()),
        None,
    )


def config_route(settings: Settings) -> dict[str, Any]:
    result: dict[str, Any] = {
        "provider": None,
        "base_url": None,
        "upstream": None,
        "route": "missing",
    }
    if not settings.config_path.exists():
        return result
    try:
        doc = tomlkit.parse(settings.config_path.read_text(encoding="utf-8"))
        provider = str(doc.get("model_provider", ""))
        table = doc.get("model_providers", {}).get(provider)
        if table is None:
            result.update(provider=provider or None, route="invalid")
            return result
        base_url = str(table.get("base_url", ""))
        headers = table.get("http_headers", {})
        key = _header_key(headers)
        upstream = str(headers.get(key, "")) if key else None
        route = "direct"
        if _same_url(base_url, settings.headroom_base) and _same_url(
            upstream or "", settings.cc_origin
        ):
            route = "bridged"
        elif _same_url(base_url, settings.cc_base):
            route = "cc-switch"
        result.update(
            provider=provider,
            base_url=base_url or None,
            upstream=upstream,
            route=route,
        )
        return result
    except (OSError, ValueError, TypeError):
        result["route"] = "invalid"
        return result


def _same_url(left: str, right: str) -> bool:
    return left.rstrip("/").casefold() == right.rstrip("/").casefold()


def _atomic_write(path: Path, original: str, updated: str) -> bool:
    if original == updated:
        return False
    stat = path.stat()
    if path.read_text(encoding="utf-8") != original:
        raise RuntimeError("config changed during reconciliation")
    fd, temp_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(updated)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(temp_name, stat.st_mode)
        os.replace(temp_name, path)
    finally:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(temp_name)
    return True


def reconcile(settings: Settings, bridge: bool) -> bool:
    settings.state_dir.mkdir(parents=True, exist_ok=True)
    lock_path = settings.state_dir / "config.lock"
    with lock_path.open("a+", encoding="utf-8") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        for attempt in range(3):
            original = settings.config_path.read_text(encoding="utf-8")
            doc = tomlkit.parse(original)
            provider = str(doc.get("model_provider", ""))
            providers = doc.get("model_providers")
            table = providers.get(provider) if providers is not None else None
            if not provider or table is None:
                raise RuntimeError("active Codex model provider is not configurable")

            headers = table.get("http_headers")
            key = _header_key(headers)
            if bridge:
                current_base = str(table.get("base_url", ""))
                if not (
                    _same_url(current_base, settings.cc_base)
                    or _same_url(current_base, settings.headroom_base)
                ):
                    raise RuntimeError(
                        f"refusing to bridge unexpected provider URL: {current_base or '<empty>'}"
                    )
                table["base_url"] = settings.headroom_base
                table["supports_websockets"] = False
                if headers is None:
                    headers = tomlkit.inline_table()
                    table["http_headers"] = headers
                headers[key or BRIDGE_HEADER] = settings.cc_origin
            else:
                current_base = str(table.get("base_url", ""))
                if not _same_url(current_base, settings.headroom_base) and key is None:
                    return False
                table["base_url"] = settings.cc_base
                table["supports_websockets"] = False
                if headers is not None and key is not None:
                    del headers[key]
                    if not list(headers.keys()):
                        del table["http_headers"]

            try:
                return _atomic_write(settings.config_path, original, tomlkit.dumps(doc))
            except RuntimeError:
                if attempt == 2:
                    raise
                time.sleep(0.1)
        return False


def snapshot(settings: Settings) -> Path:
    backup_dir = settings.state_dir / "backups"
    backup_dir.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    target = backup_dir / f"config-{stamp}.toml"
    shutil.copy2(settings.config_path, target)
    return target


def _launch_target(label: str) -> str:
    return f"gui/{os.getuid()}/{label}"


def _launchctl(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["/bin/launchctl", *args],
        check=check,
        capture_output=True,
        text=True,
    )


def _write_plist(path: Path, data: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temp_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as handle:
            plistlib.dump(data, handle, sort_keys=False)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(temp_name, 0o644)
        os.replace(temp_name, path)
    finally:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(temp_name)


def _plist(settings: Settings, label: str) -> Path:
    return settings.launch_agents_dir / f"{label}.plist"


def _service_loaded(label: str) -> bool:
    return _launchctl("print", _launch_target(label), check=False).returncode == 0


def install_services(settings: Settings, headroom_bin: str, bridge_bin: str) -> Path:
    settings.state_dir.mkdir(parents=True, exist_ok=True)
    backup = snapshot(settings)
    common = {
        "RunAtLoad": True,
        "KeepAlive": True,
        "ProcessType": "Background",
        "ThrottleInterval": 5,
        "WorkingDirectory": str(settings.home),
    }
    proxy_plist = {
        "Label": PROXY_LABEL,
        "ProgramArguments": [
            headroom_bin,
            "proxy",
            "--host",
            settings.headroom_host,
            "--port",
            str(settings.headroom_port),
        ],
        "EnvironmentVariables": {
            "HEADROOM_TELEMETRY": "off",
            "HEADROOM_STRIP_INTERNAL_HEADERS": "enabled",
        },
        "StandardOutPath": str(settings.state_dir / "headroom.log"),
        "StandardErrorPath": str(settings.state_dir / "headroom.err.log"),
        **common,
    }
    bridge_plist = {
        "Label": BRIDGE_LABEL,
        "ProgramArguments": [bridge_bin, "watch"],
        "StandardOutPath": str(settings.state_dir / "bridge.log"),
        "StandardErrorPath": str(settings.state_dir / "bridge.err.log"),
        **common,
    }
    for label, data in ((PROXY_LABEL, proxy_plist), (BRIDGE_LABEL, bridge_plist)):
        path = _plist(settings, label)
        _launchctl("bootout", _launch_target(label), check=False)
        _write_plist(path, data)
        _launchctl("bootstrap", f"gui/{os.getuid()}", str(path))
    return backup


def stop_services(settings: Settings) -> None:
    _launchctl("bootout", _launch_target(BRIDGE_LABEL), check=False)
    with contextlib.suppress(Exception):
        reconcile(settings, bridge=False)
    _launchctl("bootout", _launch_target(PROXY_LABEL), check=False)


def start_services(settings: Settings) -> None:
    for label in (PROXY_LABEL, BRIDGE_LABEL):
        path = _plist(settings, label)
        if not path.exists():
            raise RuntimeError(f"service is not installed: {label}")
        if not _service_loaded(label):
            _launchctl("bootstrap", f"gui/{os.getuid()}", str(path))
        else:
            _launchctl("kickstart", "-k", _launch_target(label))


def uninstall_services(settings: Settings) -> None:
    stop_services(settings)
    for label in (BRIDGE_LABEL, PROXY_LABEL):
        with contextlib.suppress(FileNotFoundError):
            _plist(settings, label).unlink()


def status(settings: Settings) -> dict[str, Any]:
    return {
        "headroom_ready": headroom_ready(settings),
        "cc_switch_ready": tcp_ready(settings.cc_host, settings.cc_port),
        "cc_switch_takeover": cc_takeover_enabled(settings),
        "proxy_service_loaded": _service_loaded(PROXY_LABEL),
        "bridge_service_loaded": _service_loaded(BRIDGE_LABEL),
        "config": config_route(settings),
    }


def print_status(data: dict[str, Any]) -> None:
    route = data["config"]
    checks = [
        ("Headroom", data["headroom_ready"]),
        ("CC Switch proxy", data["cc_switch_ready"]),
        ("CC Switch Codex takeover", data["cc_switch_takeover"]),
        ("Headroom LaunchAgent", data["proxy_service_loaded"]),
        ("Bridge LaunchAgent", data["bridge_service_loaded"]),
        ("Codex route", route["route"] == "bridged"),
    ]
    for name, ok in checks:
        print(f"{'OK' if ok else 'FAIL':4}  {name}")
    print(f"      provider={route['provider']} route={route['route']}")


def watch(settings: Settings, interval: float = 0.5) -> None:
    stopped = False

    def stop(_signum: int, _frame: Any) -> None:
        nonlocal stopped
        stopped = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    last_open = 0.0
    last_error = ""
    while not stopped:
        cc_ready = tcp_ready(settings.cc_host, settings.cc_port)
        takeover = cc_takeover_enabled(settings)
        hr_ready = headroom_ready(settings)
        if takeover and not cc_ready and time.monotonic() - last_open > 30:
            subprocess.run(
                ["/usr/bin/open", "-gja", "CC Switch"],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            last_open = time.monotonic()
        try:
            if cc_ready and takeover and hr_ready:
                reconcile(settings, bridge=True)
            elif cc_ready and not hr_ready:
                reconcile(settings, bridge=False)
            last_error = ""
        except Exception as exc:
            message = str(exc)
            if message != last_error:
                print(f"bridge: {message}", file=sys.stderr, flush=True)
                last_error = message
        time.sleep(interval)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="codex-headroom-bridge",
        description="Bridge Codex Desktop -> Headroom -> CC Switch.",
    )
    parser.add_argument("--config")
    parser.add_argument("--cc-db")
    parser.add_argument("--headroom-port", type=int, default=8787)
    parser.add_argument("--cc-port", type=int, default=15721)
    sub = parser.add_subparsers(dest="command", required=True)
    install = sub.add_parser("install")
    install.add_argument("--headroom-bin")
    install.add_argument("--bridge-bin")
    sub.add_parser("start")
    sub.add_parser("stop")
    sub.add_parser("status")
    sub.add_parser("doctor")
    sub.add_parser("reconcile")
    sub.add_parser("watch")
    sub.add_parser("uninstall")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    settings = default_settings(args)
    try:
        if args.command == "install":
            headroom_bin = args.headroom_bin or shutil.which("headroom")
            bridge_bin = args.bridge_bin or shutil.which("codex-headroom-bridge")
            if not headroom_bin:
                raise RuntimeError("headroom executable not found")
            if not bridge_bin:
                raise RuntimeError("codex-headroom-bridge executable not found")
            backup = install_services(settings, headroom_bin, bridge_bin)
            print(f"Installed. Config backup: {backup}")
        elif args.command == "start":
            start_services(settings)
        elif args.command == "stop":
            stop_services(settings)
        elif args.command == "uninstall":
            uninstall_services(settings)
            print(f"Removed LaunchAgents. Backups remain in {settings.state_dir / 'backups'}")
        elif args.command in {"status", "doctor"}:
            data = status(settings)
            print_status(data)
            if args.command == "doctor":
                return 0 if all(
                    (
                        data["headroom_ready"],
                        data["cc_switch_ready"],
                        data["cc_switch_takeover"],
                        data["proxy_service_loaded"],
                        data["bridge_service_loaded"],
                        data["config"]["route"] == "bridged",
                    )
                ) else 1
        elif args.command == "reconcile":
            changed = reconcile(settings, bridge=True)
            print("updated" if changed else "already bridged")
        elif args.command == "watch":
            watch(settings)
        return 0
    except Exception as exc:
        print(f"codex-headroom-bridge: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
