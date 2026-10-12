//! 后台搬运的调度与一次性执行。
//!
//! 这里是 Windows 服务与 GUI 内建线程**共用**的部分，也是 `--round` 的实现：
//! 定时策略只有一份，避免两条路径行为不一致。
//!
//! 调度刻意不用「睡满整个间隔」的写法，而是每 [`TICK`] 醒一次判断是否到期：
//! 这样合盖休眠唤醒后会立刻补跑一轮，轮次本身的耗时也不会累积成周期漂移。

use crate::{config::Settings, context::Context, paths, report::Reporter, transport};
use anyhow::{Context as _, Result};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

/// 调度心跳。服务与 GUI 每这么久醒一次，决定要不要真的跑一轮。
pub const TICK: Duration = Duration::from_secs(30);

/// 从设置里读出的调度参数。
#[derive(Clone, Copy, Debug)]
pub struct Schedule {
    pub enabled: bool,
    pub interval: Duration,
}

impl Schedule {
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            enabled: settings.background_enabled,
            // §10 风险 10：服务端有请求频率限制，下限 1 分钟、上限 24 小时。
            interval: Duration::from_secs(
                u64::from(settings.background_minutes.clamp(1, 24 * 60)) * 60,
            ),
        }
    }
}

/// 「下次到期时刻」，而不是「已经睡了多久」。
#[derive(Clone, Copy, Debug)]
pub struct DueAt {
    next: Instant,
}

impl DueAt {
    /// 立即到期：服务/程序刚启动时先拉一轮最新内容。
    pub fn now() -> Self {
        Self {
            next: Instant::now(),
        }
    }

    pub fn reschedule(&mut self, interval: Duration) {
        self.next = Instant::now() + interval;
    }

    pub fn is_due(&self) -> bool {
        Instant::now() >= self.next
    }
}

/// 无界面地跑一轮搬运。Windows 服务、后续的 macOS LaunchAgent、手工排查都用它。
pub fn run_once(
    app_dir: &Path,
    ini: &Path,
    cancel: &AtomicBool,
    reporter: &dyn Reporter,
) -> Result<String> {
    let mut settings = Settings::load(ini)?;
    let context = Context::build(app_dir, ini, &mut settings)?;
    for notice in context.notices() {
        reporter.log(notice);
    }
    let outcome = transport::round(&context, cancel, reporter)?;
    Ok(format!(
        "后台搬运完成：下载 {} 项，上传 {} 项。",
        outcome.downloaded, outcome.uploaded
    ))
}

/// 无界面地把「同步词库」完整跑一遍，这就是桌面快捷方式做的事。
///
/// 它包含 RIME 自带的用户资料同步（`/sync`）与重新部署（`/deploy`），
/// 但在那之前必须先做一遍并集与写回：否则 `Sync/Others/` 里的"其他同步文件"
/// 根本不会出现在 RIME 用户目录里，`/deploy` 也就无从生效。
pub fn sync_now(
    app_dir: &Path,
    ini: &Path,
    cancel: &AtomicBool,
    reporter: &dyn Reporter,
) -> Result<String> {
    let mut settings = Settings::load(ini)?;
    let context = Context::build(app_dir, ini, &mut settings)?;
    for notice in context.notices() {
        reporter.log(notice);
    }
    let outcome = crate::engine::manual_sync(&context, cancel, reporter)?;
    let mut message = format!(
        "同步完成：合并 {} 个文件，上传 {} 项。",
        outcome.merged, outcome.uploaded
    );
    if !outcome.conflicts.is_empty() {
        message.push_str(&format!(
            "\n\n有 {} 个文件双端都被修改过，已保留本地版本并把远端版本另存为副本：\n{}",
            outcome.conflicts.len(),
            outcome.conflicts.join("\n")
        ));
    }
    Ok(message)
}

/// 写日志文件的 [`Reporter`]：无界面模式（服务）使用，日志与 GUI 共用同一个 `RimeSync.log`。
pub struct FileReporter {
    path: PathBuf,
}

impl FileReporter {
    pub fn new(app_dir: &Path) -> Self {
        Self {
            path: app_dir.join("RimeSync.log"),
        }
    }
}

impl Reporter for FileReporter {
    fn log(&self, text: &str) {
        append_log(&self.path, &crate::report::timestamped(text));
    }
    fn progress(&self, _value: u8) {}
}

/// 追加一行日志；失败时静默（服务不能因为写不了日志而崩）。
pub fn append_log(path: &Path, line: &str) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}

/// `.rime-sync/wake` 的路径（入参是 `.rime-sync` 目录本身）。
pub fn wake_file(meta_dir: &Path) -> PathBuf {
    meta_dir.join(paths::WAKE_FILE)
}

/// UI 请求「立刻跑一轮」：不直接调服务，只丢一个标记文件，
/// 这样 GUI 与服务之间不需要任何 IPC，也天然兼容两种后台模式。
pub fn request_wake(meta_dir: &Path) -> Result<()> {
    let path = wake_file(meta_dir);
    fs::create_dir_all(meta_dir).with_context(|| format!("创建 {} 失败", meta_dir.display()))?;
    fs::write(&path, crate::identity::now_stamp())
        .with_context(|| format!("写入 {} 失败", path.display()))?;
    Ok(())
}

/// 消费一次唤醒请求：存在则删除并返回 `true`。
pub fn take_wake(meta_dir: &Path) -> bool {
    let path = wake_file(meta_dir);
    if path.is_file() {
        let _ = fs::remove_file(&path);
        true
    } else {
        false
    }
}

/// 从设置推出 `.rime-sync` 目录；用户目录还没配置时返回 `None`。
pub fn meta_dir_for_settings(settings: &Settings) -> Option<PathBuf> {
    if settings.user_data_dir.trim().is_empty() {
        return None;
    }
    Some(crate::context::resolve_rime_dir(settings).join(paths::META_DIR_NAME))
}

/// 消费设置对应的唤醒请求（服务每轮 tick 调用它）。
pub fn take_wake_for_settings(settings: &Settings) -> bool {
    meta_dir_for_settings(settings).is_some_and(|meta| take_wake(&meta))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_clamps_the_interval() {
        let mut settings = Settings {
            background_minutes: 0,
            ..Default::default()
        };
        assert_eq!(
            Schedule::from_settings(&settings).interval,
            Duration::from_secs(60)
        );
        settings.background_minutes = 100_000;
        assert_eq!(
            Schedule::from_settings(&settings).interval,
            Duration::from_secs(24 * 60 * 60)
        );
    }

    #[test]
    fn due_at_starts_due_and_reschedules_into_the_future() {
        let mut due = DueAt::now();
        assert!(due.is_due());
        due.reschedule(Duration::from_secs(3600));
        assert!(!due.is_due());
    }

    #[test]
    fn wake_request_is_consumed_once() {
        let dir = tempfile::tempdir().unwrap();
        let meta = dir.path().join(".rime-sync");
        assert!(!take_wake(&meta));
        request_wake(&meta).unwrap();
        assert!(take_wake(&meta));
        assert!(!take_wake(&meta));
    }
}
