"""Offline source-level differential contract. Never launches Chromium or Apple requests.

这里刻意有两条基线，`3d7c56e` 和上游 `04e646d`（v1.0.15）：

- `FORK_BASE = 3d7c56e`：fork 自己拥有的部分（浏览器选择）仍然逐字对照用户验过的版本。
- `UPSTREAM_BASE = 04e646d`：查询路径（CDP 准备与握手、启动超时、请求与状态分类）
  现在的负责人是上游 —— 本次合并正是为了接入它的请求节流、403/429/541 分级保护
  冷却、冷却结束后的恢复探测以及零件号分批。继续拿 `3d7c56e` 当基线会必然变红，
  但那不是回归，而是这次有意改掉的东西；拿上游当基线才能继续拦住「再有人悄悄改动
  查询语义」这类真正的漂移。

  基线从 `09a4e31` 前移到 `04e646d`：v1.0.14 `62b4b78` 把门店取货改走
  `/shop/retail/pickup-message`，送货仍走 `fulfillment-messages` 且失败不覆盖取货结论；
  v1.0.15 `04e646d` 区分海外站会话页并新增「暂停取货」。两者随本次选择性移植接入。

  `start()` 现在与上游一致（仅保留 fork 的 OwnedChild RAII 归一化）：临时 profile
  与子进程的所有权仍在 fork 手里，但会话状态结构已随上游扩展（冷却、批量复用、
  送货缓存）。

对照方式仍然是逐字相等 + 只做注释/空白的归一化，所以任何进一步的改动都会变红，
而不是静默改变查询行为。真实 RED/GREEN 证据在 Rust 侧的离线用例里：
`限流保留会话而明确拦截仍清理会话`（429 保留会话、403/541 释放临时资料）
与 `blocked_query_discards_its_profile_like_3d7c56e`。
"""
import pathlib
import re
import subprocess
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
PATH = "src-tauri/src/chromium_fetcher.rs"
FORK_BASE_REV = "3d7c56e"
UPSTREAM_BASE_REV = "04e646d"

FORK_BASE = subprocess.check_output(["git", "show", f"{FORK_BASE_REV}:{PATH}"], cwd=ROOT, text=True)
UPSTREAM_BASE = subprocess.check_output(["git", "show", f"{UPSTREAM_BASE_REV}:{PATH}"], cwd=ROOT, text=True)
CURRENT = (ROOT / PATH).read_text()

def section(source, start, end):
    return source[source.index(start):source.index(end, source.index(start))]

def normalize(source):
    # Visibility is widened for bag.rs reuse; it does not change browser selection.
    source = source.replace("pub(crate) ", "")
    return re.sub(r"\s+", "", re.sub(r"(?m)^\s*//[^\n]*", "", source))

def fork_owned_startup(source):
    """fork 的 OwnedChild RAII 归一化：所有权包装不该让基线比较失效。"""
    actual = section(source, "    async fn start()", "    async fn command(")
    actual = actual.replace("OwnedChild(Command::new(chrome)", "Command::new(chrome)")
    actual = actual.replace('format!("无法启动 Chromium：{e}")))?);', 'format!("无法启动 Chromium：{e}")))?;')
    actual = actual.replace("child.0", "child").replace("_child: child,", "child,")
    return actual

class QueryContract(unittest.TestCase):
    def test_startup_owns_child_like_upstream(self):
        self.assertEqual(normalize(fork_owned_startup(CURRENT)), normalize(section(UPSTREAM_BASE, "    async fn start()", "    async fn command(")))

    def test_cdp_preparation_readiness_and_request_match_upstream(self):
        for start, end in [("    async fn command(", "fn chromium_exception_summary"),
                           ("const CHROME_START_TIMEOUT", "type Socket"),
                           ("    async fn pickup(", "impl Fetcher")]:
            self.assertEqual(normalize(section(CURRENT, start, end)), normalize(section(UPSTREAM_BASE, start, end)))

    def test_browser_discovery_still_matches_verified_fork_baseline(self):
        for start, end in [("fn find_chromium()", "/// 可交给核心")]:
            self.assertEqual(normalize(section(CURRENT, start, end)), normalize(section(FORK_BASE, start, end)))

    def test_no_persistent_profile_or_legacy_directory_cleanup(self):
        production = CURRENT.split("#[cfg(test)]\nmod tests")[0]
        for token in ["chrome-query", "claim_profile", "generation-", "cleanup_stale_profiles", "remove_dir_all", "read_dir(", "Browser.close"]:
            self.assertNotIn(token, production)

if __name__ == "__main__":
    unittest.main()
