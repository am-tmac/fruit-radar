"""Offline differential contract after user-authorized v1.0.19 query migration.

Browser discovery stays pinned to 3d7c56e. CDP/request preparation is pinned to
f86fee4 (v1.0.19): real browser identity, persistent app-only profile and nearby
query planning were explicitly authorized. The obsolete 04e646d temporary-profile
startup/403 destruction assertions are replaced, not silently normalized away:
Rust local-CDP tests verify retained rejection sessions, owned-child shutdown,
exclusive profile release and preserved cookies. No personal profile migration,
legacy process reclamation or system temp scan is permitted.
"""
import pathlib
import re
import subprocess
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
PATH = "src-tauri/src/chromium_fetcher.rs"
FORK_BASE_REV = "3d7c56e"
UPSTREAM_BASE_REV = "f86fee4"
FORK_BASE = subprocess.check_output(["git", "show", f"{FORK_BASE_REV}:{PATH}"], cwd=ROOT, text=True)
UPSTREAM_BASE = subprocess.check_output(["git", "show", f"{UPSTREAM_BASE_REV}:{PATH}"], cwd=ROOT, text=True)
CURRENT = (ROOT / PATH).read_text()

def section(source, start, end):
    return source[source.index(start):source.index(end, source.index(start))]

def browser_discovery(source):
    start = source.index("fn find_chromium()")
    end = source.index("\n}\n", start) + len("\n}")
    return source[start:end]

def normalize(source):
    source = source.replace("pub(crate) ", "")
    return re.sub(r"\s+", "", re.sub(r"(?m)^\s*//[^\n]*", "", source))

class QueryContract(unittest.TestCase):
    def test_cdp_preparation_readiness_and_request_match_authorized_upstream(self):
        for start, end in [("    async fn command(", "fn chromium_exception_summary"),
                           ("fn brand_list(", "impl ChromiumSession"),
                           ("fn pickup_query_pairs(", "fn delivery_query_pairs("),
                           ("fn delivery_query_pairs(", "fn find_chromium()"),
                           ("fn plan_nearby_parts(", "#[derive(Debug, Default)]\nstruct RequestGate")]:
            self.assertEqual(normalize(section(CURRENT, start, end)), normalize(section(UPSTREAM_BASE, start, end)))

    def test_browser_discovery_still_matches_verified_fork_baseline(self):
        self.assertEqual(normalize(browser_discovery(CURRENT)), normalize(browser_discovery(FORK_BASE)))

    def test_persistent_profile_is_app_only_and_exclusively_owned(self):
        production = CURRENT.split("#[cfg(test)]\nmod tests")[0]
        self.assertIn('const CHROMIUM_PROFILE_DIR: &str = "chromium-profile-v1"', production)
        self.assertIn('ProfileOwner::claim(&profile_dir)', production)
        self.assertIn('_owner: Some(owner)', production)
        self.assertLess(production.index('_process: ChromiumProcess'), production.index('_owner: Option'))
        for token in ["chrome-query", "generation-", "cleanup_stale_profiles", "reclaim_orphaned_profile_process", "remove_dir_all", "read_dir(", "Browser.close", "Google/Chrome/Default"]:
            self.assertNotIn(token, production)

    def test_default_protection_and_strict_opt_in_are_wired(self):
        config = (ROOT / "crates/apw-core/src/config.rs").read_text()
        host = (ROOT / "src-tauri/src/lib.rs").read_text()
        self.assertIn('backoff_enabled: true', config)
        self.assertIn('set_backoff_enabled(&state.fetcher, next.backoff_enabled)', host)
        self.assertIn('set_backoff_enabled(&fetcher, backoff_enabled)', host)
        self.assertIn(normalize('if !guard.strict_interval && let Some(error) = guard.active_cooldown_error(region)'), normalize(CURRENT))

if __name__ == "__main__":
    unittest.main()
