//! 活动日志落盘。
//!
//! 界面上的那份日志只活在内存里，应用一关就没了。落盘这份唯一的用途是事后回溯：
//! 「刚才那个 541 是几点冒出来的」。所以这里按大小滚动、只保留最近若干行 ——
//! 一个长到没人愿意打开的文件，等于没有日志。

use std::io::Write;
use std::path::Path;

/// 文件超过这个大小就滚动一次。
pub const MAX_BYTES: u64 = 512 * 1024;

/// 滚动后的目标大小：上限的一半。
///
/// 直接留满上限，下一次追加立刻又超限 —— 结果就是每写一行都要把整个文件重写一遍。
/// 掉到一半再涨回来，重写才变成一件偶发的事。
const TRIM_TARGET_BYTES: usize = (MAX_BYTES / 2) as usize;

/// 滚动后保留的最大行数。
///
/// 字节上限管不到「短行」：几万条十几字节的日志加起来也才几百 KB，但没人愿意翻。
/// 这一条是给可读性兜底的。
pub const KEEP_LINES: usize = 1000;

/// 把若干行追加到日志文件；超过 [`MAX_BYTES`] 时滚动。
///
/// 只报错、不重试：日志写不进去不该反过来影响监控本身 —— 到货提醒才是这程序存在
/// 的理由，为一条日志卡住或反复重试都是本末倒置。
pub fn append(path: &Path, lines: &[String]) -> std::io::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // A stable sidecar lock covers append AND rotation across threads/processes.
    let lock = std::fs::OpenOptions::new().create(true).truncate(false)
        .write(true).open(path.with_extension("log.lock"))?;
    lock.lock()?;
    let mut block = lines.iter().rev().take(KEEP_LINES).map(|line| sanitize(line))
        .collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
    block.push('\n');
    let existing = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if existing + block.len() as u64 > MAX_BYTES {
        let mut content = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        content.push_str(&block);
        let trimmed = tail_with_budget(&content, KEEP_LINES, TRIM_TARGET_BYTES);
        let mut tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))?;
        tmp.write_all(trimmed.as_bytes())?;
        tmp.persist(path).map_err(|e| e.error)?;
    } else {
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        file.write_all(block.as_bytes())?;
    }
    Ok(())
}

/// 从后往前留：最多 `keep_lines` 行，且累计字节不超过 `budget`。
///
/// 用「读全文 → 取尾 → 覆写」而不是边读边删：文件上限只有几百 KB，代价可以忽略，
/// 而覆写让「滚动之后文件里到底剩什么」一目了然。
#[cfg(test)]
fn trim(path: &Path, keep_lines: usize, budget: usize) -> std::io::Result<()> {
    std::fs::write(path, tail_with_budget(&std::fs::read_to_string(path)?, keep_lines, budget))
}

fn tail_with_budget(content: &str, keep_lines: usize, budget: usize) -> String {

    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    for line in content.lines().rev() {
        if kept.len() >= keep_lines {
            break;
        }
        let cost = line.len() + 1; // 加上换行符本身
        // Never exceed the budget, including a single oversized legacy line.
        if bytes + cost > budget {
            break;
        }
        bytes += cost;
        kept.push(line);
    }

    if kept.is_empty() { String::new() }
    else { kept.into_iter().rev().collect::<Vec<_>>().join("\n") + "\n" }
}

/// Defense in depth at the persistence boundary: never persist URL credentials.
pub fn sanitize(line: &str) -> String {
    let mut out = String::new();
    for word in line.split_whitespace() {
        if !out.is_empty() { out.push(' '); }
        if let Some(at) = word.find("https://").or_else(|| word.find("http://")) {
            out.push_str(&word[..at]);
            out.push_str("[URL 已隐藏]");
        } else { out.push_str(word); }
        if out.len() >= 8192 { break; }
    }
    let mut end = out.len().min(8192);
    while !out.is_char_boundary(end) { end -= 1; }
    out.truncate(end);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(from: usize, to: usize, prefix: &str) -> Vec<String> {
        (from..=to).map(|i| format!("[{i:05}] {prefix}")).collect()
    }

    fn content(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn concurrent_append_keeps_every_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity.log");
        std::thread::scope(|scope| {
            for i in 0..16 {
                let path = &path;
                scope.spawn(move || { for j in 0..20 { append(path, &[format!("{i}-{j}")]).unwrap(); } });
            }
        });
        let text = content(&path);
        assert_eq!(text.lines().collect::<std::collections::HashSet<_>>().len(), 320);
    }

    #[test]
    fn oversized_utf8_line_is_bounded_and_urls_are_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity.log");
        append(&path, &[format!("failed https://api.day.app/secret-key/title {}", "果".repeat(MAX_BYTES as usize))]).unwrap();
        let text = content(&path);
        assert!(text.len() <= MAX_BYTES as usize);
        assert!(!text.contains("secret-key"));
    }

    #[test]
    fn appends_without_losing_earlier_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity.log");
        append(&path, &numbered(1, 2, "第一轮")).unwrap();
        append(&path, &numbered(1, 2, "第二轮")).unwrap();

        let text = content(&path);
        assert_eq!(text.lines().count(), 4);
        assert_eq!(text.lines().next().unwrap(), "[00001] 第一轮");
        assert_eq!(text.lines().last().unwrap(), "[00002] 第二轮");
    }

    #[test]
    fn empty_batch_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity.log");
        append(&path, &[]).unwrap();
        // 连文件都不该建：空文件会让用户以为「日志是空的」，而其实是还没开始记。
        assert!(!path.exists());
    }

    #[test]
    fn creates_missing_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/activity.log");
        append(&path, &numbered(1, 1, "首次写入")).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn trim_leaves_a_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity.log");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        trim(&path, 2, TRIM_TARGET_BYTES).unwrap();
        assert_eq!(content(&path), "b\nc\n");
    }

    #[test]
    fn long_lines_stay_within_the_byte_budget() {
        // 一行很长时，行数上限毫无意义：1000 行 × 1KB 会比上限还大一倍。
        // 若还按行数留，文件永远超限 → 每次追加都重写全文，日志越用越慢。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity.log");
        let big = "x".repeat(1024);

        let mut written = 0usize;
        while written < 4000 {
            written += 1;
            append(&path, &[format!("[{written:05}] {big}")]).unwrap();
            let size = std::fs::metadata(&path).unwrap().len();
            assert!(size <= MAX_BYTES, "第 {written} 次追加后仍有 {size} 字节，滚动没生效");
        }

        assert!(
            std::fs::metadata(&path).unwrap().len() <= MAX_BYTES,
            "收尾时仍在超限状态"
        );
        // 留下的必须是**最新**的，否则滚动等于把有用的部分删了。
        assert!(content(&path)
            .lines()
            .last()
            .unwrap()
            .starts_with(&format!("[{written:05}]")));
    }

    #[test]
    fn line_cap_keeps_the_newest_lines() {
        // 行很短时字节上限管不着（要几万行才到 512 KB），这时唯一的闸门是行数。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity.log");

        let total = KEEP_LINES * 3;
        for chunk_start in (1..=total).step_by(500) {
            let chunk_end = (chunk_start + 499).min(total);
            append(&path, &numbered(chunk_start, chunk_end, "短行")).unwrap();
        }
        // 把文件撑过上限，逼出一次滚动。
        let filler = "f".repeat(4096);
        for i in 0..200 {
            append(&path, &[format!("[填满 {i:03}] {filler}")]).unwrap();
        }

        let text = content(&path);
        let kept = text.lines().count();
        // 行数上限只在滚动那一刻执行，两次滚动之间还会再堆一点零头。
        // 这里 4 KB 一行，零头顶多几十行，给 100 行的余量足够说明问题。
        assert!(kept <= KEEP_LINES + 100, "滚动后留下 {kept} 行，远超行数上限");
        assert!(kept > 0, "滚动把日志清空了");
        assert!(kept < total, "根本没发生过滚动");
        assert!(
            text.lines().last().unwrap().starts_with("[填满 199]"),
            "最新的那一行没留下"
        );
    }
}
