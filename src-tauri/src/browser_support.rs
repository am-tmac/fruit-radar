//! 与平台无关的浏览器启动错误文案（移植自上游 v1.0.21，去掉 Windows 进程归属判断）。

/// 把浏览器启动后立刻退出的情况翻译成用户能处理的原因。
///
/// 只按退出码和 stderr 判断，不猜测是谁占用了资料目录，也不触发任何清理。
pub(crate) fn startup_exit_detail(code: Option<i32>, stderr: &str) -> String {
    let lower = stderr.to_ascii_lowercase();
    let permission_denied = lower.contains("access is denied")
        || lower.contains("permission denied")
        || lower.contains("operation not permitted")
        || lower.contains("拒绝访问");
    let message = if permission_denied {
        "系统拒绝访问浏览器专属资料目录。请检查目录权限和安全软件的拦截记录"
    } else if code == Some(21) {
        "浏览器专属资料目录被占用，或无法取得目录锁。请确认没有其他水果雷达实例正在运行"
    } else {
        "查询浏览器启动后退出，尚未取得库存。请查看启动诊断，确认 Chrome 或 Edge 可以正常运行"
    };
    match code {
        Some(code) => format!("{message}（退出码 {code}）"),
        None => message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_21_does_not_guess_who_owns_the_profile() {
        let detail = startup_exit_detail(Some(21), "");
        assert!(detail.contains("被占用，或无法取得目录锁"));
        assert!(detail.contains("退出码 21"));
        assert!(!detail.contains("已清理"));
        assert!(startup_exit_detail(Some(21), "Access is denied").contains("系统拒绝访问"));
        assert!(startup_exit_detail(None, "Operation not permitted").contains("系统拒绝访问"));
        assert!(startup_exit_detail(Some(1), "").contains("启动后退出"));
    }
}
