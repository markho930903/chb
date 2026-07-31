import sqlite3
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import tomlkit

from codex_headroom_bridge import Settings, config_route, reconcile, stop_services, watch


class BridgeTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        root = Path(self.temp.name)
        self.config = root / "config.toml"
        self.db = root / "cc-switch.db"
        self.settings = Settings(
            home=root,
            config_path=self.config,
            cc_db_path=self.db,
            state_dir=root / "state",
            launch_agents_dir=root / "LaunchAgents",
        )
        self.config.write_text(
            '''# keep this comment
model_provider = "custom"
model = "gpt-5.6-sol"

[model_providers.custom]
name = "CCTQ"
base_url = "http://127.0.0.1:15721/v1"
wire_api = "responses"
experimental_bearer_token = "PROXY_MANAGED"

[features]
memories = true
''',
            encoding="utf-8",
        )

    def tearDown(self):
        self.temp.cleanup()

    def test_bridge_preserves_provider_auth_and_unrelated_config(self):
        self.assertTrue(reconcile(self.settings, bridge=True))
        text = self.config.read_text(encoding="utf-8")
        doc = tomlkit.parse(text)
        provider = doc["model_providers"]["custom"]
        self.assertEqual(doc["model_provider"], "custom")
        self.assertEqual(provider["base_url"], "http://127.0.0.1:8787/v1")
        self.assertEqual(
            provider["http_headers"]["X-Headroom-Base-Url"],
            "http://127.0.0.1:15721",
        )
        self.assertFalse(provider["supports_websockets"])
        self.assertEqual(provider["experimental_bearer_token"], "PROXY_MANAGED")
        self.assertTrue(doc["features"]["memories"])
        self.assertIn("# keep this comment", text)
        self.assertEqual(config_route(self.settings)["route"], "bridged")

    def test_bypass_only_removes_bridge_route(self):
        reconcile(self.settings, bridge=True)
        self.assertTrue(reconcile(self.settings, bridge=False))
        doc = tomlkit.parse(self.config.read_text(encoding="utf-8"))
        provider = doc["model_providers"]["custom"]
        self.assertEqual(provider["base_url"], "http://127.0.0.1:15721/v1")
        self.assertNotIn("http_headers", provider)
        self.assertEqual(provider["experimental_bearer_token"], "PROXY_MANAGED")
        self.assertEqual(config_route(self.settings)["route"], "cc-switch")

    def test_refuses_unexpected_direct_provider(self):
        doc = tomlkit.parse(self.config.read_text(encoding="utf-8"))
        doc["model_providers"]["custom"]["base_url"] = "https://provider.example/v1"
        self.config.write_text(tomlkit.dumps(doc), encoding="utf-8")
        with self.assertRaisesRegex(RuntimeError, "unexpected provider URL"):
            reconcile(self.settings, bridge=True)

    @patch("codex_headroom_bridge._launchctl")
    def test_stop_bypasses_headroom_when_cc_switch_is_offline(self, _launchctl):
        reconcile(self.settings, bridge=True)
        stop_services(self.settings)
        self.assertEqual(config_route(self.settings)["route"], "cc-switch")

    def test_watch_does_not_launch_cc_switch_without_takeover(self):
        with (
            patch("codex_headroom_bridge.signal.signal"),
            patch("codex_headroom_bridge.tcp_ready", return_value=False),
            patch("codex_headroom_bridge.cc_takeover_enabled", return_value=False),
            patch("codex_headroom_bridge.headroom_ready", return_value=True),
            patch("codex_headroom_bridge.subprocess.run") as run,
            patch("codex_headroom_bridge.time.sleep", side_effect=KeyboardInterrupt),
        ):
            with self.assertRaises(KeyboardInterrupt):
                watch(self.settings)
        run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
