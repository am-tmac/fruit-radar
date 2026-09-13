"""Offline source-level differential contract. Never launches Chromium or Apple requests."""
import pathlib
import re
import subprocess
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
PATH = "src-tauri/src/chromium_fetcher.rs"
BASE = subprocess.check_output(["git", "show", "3d7c56e:" + PATH], cwd=ROOT, text=True)
CURRENT = (ROOT / PATH).read_text()

def section(source, start, end):
    return source[source.index(start):source.index(end, source.index(start))]

def normalize(source):
    # Visibility is widened for bag.rs reuse; it does not change browser selection.
    source = source.replace("pub(crate) ", "")
    return re.sub(r"\s+", "", re.sub(r"(?m)^\s*//[^\n]*", "", source))

class QueryContract(unittest.TestCase):
    def test_startup_matches_except_owned_child_raii(self):
        start, end = "    async fn start()", "    async fn command("
        actual = section(CURRENT, start, end)
        actual = actual.replace("OwnedChild(Command::new(chrome)", "Command::new(chrome)")
        actual = actual.replace('format!("无法启动 Chromium：{e}")))?);', 'format!("无法启动 Chromium：{e}")))?;')
        actual = actual.replace("child.0", "child").replace("_child: child,", "child,")
        self.assertEqual(normalize(actual), normalize(section(BASE, start, end)))

    def test_cdp_preparation_readiness_and_request_match(self):
        for start, end in [("    async fn command(", "fn chromium_exception_summary"),
                           ("const CHROME_START_TIMEOUT", "type Socket"),
                           ("fn find_chromium()", "/// 可交给核心"),
                           ("    async fn pickup(", "impl Fetcher")]:
            self.assertEqual(normalize(section(CURRENT, start, end)), normalize(section(BASE, start, end)))

    def test_no_persistent_profile_or_legacy_directory_cleanup(self):
        production = CURRENT.split("#[cfg(test)]\nmod tests")[0]
        for token in ["chrome-query", "claim_profile", "generation-", "cleanup_stale_profiles", "remove_dir_all", "read_dir(", "Browser.close"]:
            self.assertNotIn(token, production)

if __name__ == "__main__":
    unittest.main()
