//! 后台搬运的调度与一次性执行。
//!
//! 这里是 Windows 服务与 GUI 内建线程**共用**的部分，也是 `--round` 的实现：
//! 定时策略只有一份，避免两条路径行为不一致。
//!
//! 调度刻意不用「睡满整个间隔」的写法，而是每 [`TICK`] 醒一次判断是否到期：
//! 这样合盖休眠唤醒后会立刻补跑一轮，轮次本身的耗时也不会累积成周期漂移。

use crate::{config::Settings, context::Context, paths, report::Reporter};
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

/// 无界面地跑一轮后台同步：拉取 → 并集合并写回 → 上传。
///
/// Windows 服务、macOS LaunchAgent、Linux systemd timer、手工排查都用它。
/// 不做 RIME 的 `/sync` 与 `/deploy`（那两步需要用户登录会话，见
/// [`crate::engine::background_round`]）。
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
    let outcome = crate::engine::background_round(&context, cancel, reporter)?;
    Ok(format!(
        "后台搬运完成：下载 {} 项，上传 {} 项。",
        outcome.downloaded, outcome.uploaded
    ))
}

/// 无界面地把「同步词库」完整跑一遍：搬运 + 并集合并写回 + RIME `/sync` + `/deploy`。
///
/// 它包含 RIME 自带的用户资料同步（`/sync`）与重新部署（`/deploy`），
/// 在那之前还会做一遍并集与写回：否则 `Sync/Others/` 里的"其他同步文件"
/// 根本不会出现在 RIME 用户目录里，`/deploy` 也就无从生效。
///
/// 桌面快捷方式**不用**它 —— 快捷方式只补齐服务做不到的两步，见 [`rime_sync`]。
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

/// 只补齐后台服务做不到的两步：RIME 自带的用户资料同步与重新部署。
///
/// 桌面快捷方式用的就是它 —— 与 macOS 菜单栏图标同一策略。文件拉取与上传由后台
/// 服务（[`run_once`]）完成，文本配置合并写回仍需完整同步，快捷方式不等待网络。
pub fn rime_sync(
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
    crate::engine::rime_sync_and_deploy(&context, cancel, reporter)?;
    Ok("RIME 用户资料同步完成，并已重新部署。".to_owned())
}

/// 菜单栏图标右键的「退出」：连后台搬运一起停掉，且下次登录不再恢复。
///
/// 与「隐藏图标」的区别就在后者：隐藏只是终止本进程，后台搬运照旧，
/// plist 也还在（重新运行 App 就能把图标 kickstart 回来，不必等下次登录）；
/// 退出则要把定时搬运的任务一并卸载、plist / unit 一起删掉。
///
/// 顺带把 ini 的 `[background] enabled` 写回 false，否则 GUI 重启后会显示
/// 「已开启」，而 launchd / systemd 里其实什么都没有。
pub fn quit_all(ini: &Path) -> Result<String> {
    let mut message = stop_background()?;
    if let Ok(mut settings) = Settings::load(ini) {
        settings.background_enabled = false;
        if let Err(error) = settings.save(ini) {
            message.push_str(&format!("\n警告：同步已停止，但回写配置失败：{error}"));
        }
    }
    Ok(message)
}

/// 停掉本平台的定时搬运任务，返回给界面显示的结果文案。
///
/// 三个平台各自实现而不是在 [`quit_all`] 里用 `cfg` 分支：Windows 分支只能
/// 直接返回错误，写成同一条控制流会让后面的「回写配置」变成不可达代码
/// （`-D warnings` 下 `unreachable_code` + `diverging_sub_expression` 直接报错）。
#[cfg(target_os = "macos")]
fn stop_background() -> Result<String> {
    crate::agent::uninstall()
}

#[cfg(target_os = "linux")]
fn stop_background() -> Result<String> {
    crate::service_linux::uninstall()
}

/// Windows 没有菜单栏助手（也不该由它触发）：退出界面用窗口的关闭按钮。
#[cfg(target_os = "windows")]
fn stop_background() -> Result<String> {
    Err(anyhow::anyhow!(
        "Windows 没有菜单栏助手；要停止后台搬运请在界面里点「停止后台服务」。"
    ))
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

/// 与同步上下文使用相同的目录解析规则，包括平台默认目录。
pub fn meta_dir_for_settings(settings: &Settings) -> Option<PathBuf> {
    let rime = crate::context::resolve_rime_dir(settings);
    (!rime.as_os_str().is_empty()).then(|| rime.join(paths::META_DIR_NAME))
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

    #[test]
    fn default_user_directory_has_the_same_wake_location_as_sync() {
        let settings = Settings::default();
        assert_eq!(
            meta_dir_for_settings(&settings),
            Some(crate::context::resolve_rime_dir(&settings).join(paths::META_DIR_NAME))
        );
    }
}
