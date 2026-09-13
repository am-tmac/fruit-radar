//! 记住「上次退出时在不在监控」，下次启动照原样接着跑。
//!
//! 为什么要单独一个文件、而不是塞进设置里：这是**运行状态**，不是用户偏好。用户
//! 不会想在设置界面里看到「要自动恢复吗」—— 他只希望昨天在跑，今天打开还在跑。
//! 塞进设置还会逼着前端把它一起回传，任何一次保存漏掉这个字段都会静默抹掉状态。
//!
//! 为什么值得单独做这件事：不做的话，重启（更新、崩溃、重开）之后监控是**静默
//! 暂停**的 —— 界面看着一切正常，实际一次查询都不发。对「挂着等开售」的用法，
//! 这是最要命的失效方式：等发现时，货早就没了。

use std::path::Path;

/// 读出上次的运行状态。
///
/// 文件不存在、内容看不懂，一律当作「没在跑」。猜错两个方向的代价不对称：多跑
/// 一轮只是浪费一次查询，少跑一轮可能错过整场开售；所以只承认明确的「在跑」。
pub fn read(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|text| text.trim() == "running")
        .unwrap_or(false)
}

/// 记住本次的运行状态。
///
/// 用「写临时文件 + 改名」而不是直接覆写：覆写到一半被杀（正是重启场景）会留下
/// 半截文件，下次启动读到什么就成了掷骰子。改名在同一个文件系统上是原子的。
pub fn write(path: &Path, running: bool) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::io::Write;
    let mut tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))?;
    tmp.write_all(if running { b"running\n" } else { b"stopped\n" })?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_writes_do_not_share_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("monitoring.state");
        std::thread::scope(|scope| {
            for i in 0..16 {
                let path = &path;
                scope.spawn(move || { for _ in 0..50 { write(path, i % 2 == 0).unwrap(); } });
            }
        });
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text == "running\n" || text == "stopped\n");
    }

    #[test]
    fn missing_file_means_not_running() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!read(&dir.path().join("monitoring.state")));
    }

    #[test]
    fn unreadable_content_means_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("monitoring.state");
        // 半截文件、别的工具的垃圾、用户手改错，都走这条路：宁可多问一次，
        // 也不要因为读不懂就假装在监控。
        std::fs::write(&path, "\u{0}半截").unwrap();
        assert!(!read(&path));
        std::fs::write(&path, "true").unwrap();
        assert!(!read(&path), "只认自己写下的那个词");
    }

    #[test]
    fn round_trips_both_states() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("monitoring.state");

        write(&path, true).unwrap();
        assert!(read(&path));
        write(&path, false).unwrap();
        assert!(!read(&path));
        write(&path, true).unwrap();
        assert!(read(&path), "从暂停切回监控也要记住");
    }

    #[test]
    fn creates_missing_parent_and_leaves_no_debris() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/monitoring.state");
        write(&path, true).unwrap();
        assert!(path.exists());
        // 临时文件必须被改名带走，否则每次启动都会多留下一坨残渣。
        assert!(!path.with_extension("tmp").exists());
        assert_eq!(std::fs::read_dir(dir.path().join("nested")).unwrap().count(), 1);
    }

    #[test]
    fn failure_to_write_is_not_fatal() {
        // 目录不可写时只报错、不 panic：记不住状态是小事，为此让应用起不来是大事。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-such-dir/readonly/monitoring.state");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut perms = std::fs::metadata(path.parent().unwrap()).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(path.parent().unwrap(), perms).unwrap();
        if std::fs::write(path.parent().unwrap().join("probe"), "x").is_ok() {
            // root 下只读目录照样能写，这条断言在这里就没意义了。
            return;
        }
        assert!(write(&path, true).is_err());
    }
}
