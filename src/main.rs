#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

/// macOS 的后台代理：launchd LaunchAgent，定时跑 `--scheduled-round`。
#[cfg(target_os = "macos")]
mod agent;
mod archive;
mod config;
mod context;
mod daemon;
mod device;
mod engine;
mod hash;
mod identity;
mod merge;
mod paths;
/// Linux 用自绘目录选择器：原生对话框不列隐藏项，而 RIME 目录本身就是隐藏路径。
#[cfg(target_os = "linux")]
mod picker_linux;
mod platform;
mod report;
/// 后台服务只在 Windows 上提供；桌面快捷方式两端都有（Windows `.lnk` / macOS `.app`）。
#[cfg(target_os = "windows")]
mod service;
/// Linux 没有 Windows 服务，用 systemd 用户级 timer 定时跑 `--scheduled-round`。
#[cfg(target_os = "linux")]
mod service_linux;
#[cfg(target_os = "windows")]
mod shortcut;
/// Linux 用 freedesktop `.desktop` 菜单项提供同名快捷方式。
#[cfg(target_os = "linux")]
mod shortcut_linux;
/// macOS 用 AppleScript applet 提供同名快捷方式。
#[cfg(target_os = "macos")]
mod shortcut_macos;
mod state;
mod transport;
mod ui;
mod webdav;

use anyhow::{Result, bail};
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

const USAGE: &str = "\
RIME 用户词库同步工具

不带参数启动图形界面；带参数时进入无界面模式（结果写入配置目录下的 RimeSync.log）：

  --sync-now             执行一次完整「同步词库」：拉取 → 并集 → 写回 → 上传
                         → RIME /sync → /deploy
  --rime-sync            只做 RIME 自带的用户资料同步 + 重新部署
                         （桌面快捷方式用的就是它；文件搬运由后台服务负责）
  --round                只执行一轮后台搬运后退出（不合并、不部署）
  --scheduled-round      供 macOS 后台代理调用：后台搬运关闭时跳过，否则跑一轮
  --install-service      安装并启动后台搬运：Windows 为 SCM 服务（需管理员权限），
                         Linux 为 systemd 用户级 timer（不需要提权）
  --uninstall-service    停止并卸载后台搬运
  --install-agent        安装 macOS 后台代理（launchd 定时搬运，不需要程序常开）；
                         Linux 上等同 --install-service
  --uninstall-agent      卸载 macOS 后台代理
  --install-shortcut     创建「同步词库」快捷方式
                         （Windows 为 .lnk，macOS 为 AppleScript .app，Linux 为菜单项）
  --uninstall-shortcut   删除该快捷方式
  --run-service          由服务控制管理器调用，不应手工执行
  --service-status       打印后台服务状态
  --notify               结束时弹消息框（快捷方式用；命令行下不需要）
  --app-dir <目录>       指定配置目录（默认是程序所在目录）
  --help                 显示本帮助

退出码：0 成功，1 执行失败，2 参数错误。
";

const NOTIFY_TITLE: &str = "同步词库";

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if !arguments.is_empty() {
        std::process::exit(run_cli(&arguments));
    }
    if let Err(error) = ui::run() {
        // GUI 子系统下 stderr 通常没有去处，保留这行便于从命令行启动时排查。
        eprintln!("{error}");
        std::process::exit(1);
    }
}

enum Command {
    SyncNow,
    RimeSync,
    Round,
    ScheduledRound,
    InstallService,
    UninstallService,
    InstallAgent,
    UninstallAgent,
    InstallShortcut,
    UninstallShortcut,
    RunService,
    ServiceStatus,
    Help,
}

struct Options {
    command: Command,
    app_dir: PathBuf,
    notify: bool,
}

fn parse(arguments: &[String]) -> Result<Options> {
    let mut command = None;
    let mut app_dir = None;
    let mut notify = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--sync-now" => command = Some(Command::SyncNow),
            "--rime-sync" => command = Some(Command::RimeSync),
            "--round" => command = Some(Command::Round),
            "--scheduled-round" => command = Some(Command::ScheduledRound),
            "--install-service" => command = Some(Command::InstallService),
            "--uninstall-service" => command = Some(Command::UninstallService),
            "--install-agent" => command = Some(Command::InstallAgent),
            "--uninstall-agent" => command = Some(Command::UninstallAgent),
            "--install-shortcut" => command = Some(Command::InstallShortcut),
            "--uninstall-shortcut" => command = Some(Command::UninstallShortcut),
            "--run-service" => command = Some(Command::RunService),
            "--service-status" => command = Some(Command::ServiceStatus),
            "--notify" => notify = true,
            "--help" | "-h" => command = Some(Command::Help),
            "--app-dir" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("--app-dir 后面缺少目录"))?;
                if value.trim().is_empty() {
                    bail!("--app-dir 目录不能为空或仅包含空白字符");
                }
                app_dir = Some(PathBuf::from(value));
            }
            other => bail!("未知参数 {other}；用 --help 查看用法"),
        }
        index += 1;
    }
    Ok(Options {
        command: command.unwrap_or(Command::Help),
        app_dir: match app_dir {
            Some(dir) => dir,
            None => config::app_dir()?,
        },
        notify,
    })
}

fn run_cli(arguments: &[String]) -> i32 {
    let options = match parse(arguments) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("{error:#}");
            return 2;
        }
    };
    if let Command::Help = options.command {
        println!("{USAGE}");
        return 0;
    }
    let log = options.app_dir.join("RimeSync.log");
    match dispatch(options.command, &options.app_dir) {
        Ok(message) => {
            // `--scheduled-round` 在后台搬运关闭时返回空串：不要往日志里写噪音。
            if !message.is_empty() {
                log_line(&log, &message);
                println!("{message}");
                if options.notify {
                    platform::notify(NOTIFY_TITLE, &message, false);
                }
            }
            0
        }
        Err(error) => {
            let full = platform::attach_rime_diagnostics(&format!("失败: {error:#}"));
            // 终端和日志给全量（含 stderr 与候选明细），系统通知只给一句能照做的：
            // 通知框里塞十来行扫描结果没人看，也看不清。
            log_line(&log, &full);
            eprintln!("{full}");
            if options.notify {
                platform::notify(NOTIFY_TITLE, &platform::short_error(&full), true);
            }
            1
        }
    }
}

fn dispatch(command: Command, app_dir: &Path) -> Result<String> {
    let ini = app_dir.join("RimeUserDictSync.ini");
    match command {
        Command::Help => Ok(USAGE.to_owned()),
        Command::SyncNow => {
            let reporter = daemon::FileReporter::new(app_dir);
            let cancel = AtomicBool::new(false);
            daemon::sync_now(app_dir, &ini, &cancel, &reporter)
        }
        Command::RimeSync => {
            let reporter = daemon::FileReporter::new(app_dir);
            let cancel = AtomicBool::new(false);
            daemon::rime_sync(app_dir, &ini, &cancel, &reporter)
        }
        Command::Round => {
            let reporter = daemon::FileReporter::new(app_dir);
            let cancel = AtomicBool::new(false);
            daemon::run_once(app_dir, &ini, &cancel, &reporter)
        }
        // 周期触发尊重开关；显式 wake 请求即使关闭后台活动也执行一次。
        Command::ScheduledRound => {
            let settings = config::Settings::load(&ini)?;
            let wake = daemon::take_wake_for_settings(&settings);
            if !settings.background_enabled && !wake {
                return Ok(String::new());
            }
            let reporter = daemon::FileReporter::new(app_dir);
            let cancel = AtomicBool::new(false);
            daemon::run_once(app_dir, &ini, &cancel, &reporter)
        }
        Command::ServiceStatus => service_status_text(app_dir),
        // 下面这些只在 Windows 上存在。整条 match 分支用 cfg 包住 ——
        // 不能把 `service::…` 塞进闭包再交给一个通用辅助函数：
        // 闭包体在 macOS 上照样要参与编译，而那时 `mod service` 根本不存在。
        #[cfg(target_os = "windows")]
        Command::InstallService => service::install(app_dir),
        #[cfg(target_os = "windows")]
        Command::UninstallService => service::uninstall(),
        #[cfg(target_os = "windows")]
        Command::InstallShortcut => shortcut::install(app_dir),
        #[cfg(target_os = "windows")]
        Command::UninstallShortcut => shortcut::uninstall(app_dir),
        #[cfg(target_os = "windows")]
        Command::RunService => service::run_dispatcher().map(|()| "服务已停止。".to_owned()),
        // macOS：没有 Windows 服务，用 launchd LaunchAgent 定时跑 `--scheduled-round`。
        #[cfg(target_os = "macos")]
        Command::InstallAgent => agent::install(app_dir, settings_minutes(app_dir)),
        #[cfg(target_os = "macos")]
        Command::UninstallAgent => agent::uninstall(),
        #[cfg(target_os = "macos")]
        Command::InstallShortcut => shortcut_macos::install(app_dir),
        #[cfg(target_os = "macos")]
        Command::UninstallShortcut => shortcut_macos::uninstall(app_dir),
        #[cfg(target_os = "macos")]
        Command::InstallService | Command::UninstallService | Command::RunService => {
            let _ = app_dir;
            bail!("后台服务只在 Windows 上提供；macOS 请使用 --install-agent（launchd 代理）")
        }
        #[cfg(target_os = "windows")]
        Command::InstallAgent | Command::UninstallAgent => {
            let _ = app_dir;
            bail!("后台代理只在 macOS 上提供；Windows 请使用 --install-service")
        }
        // Linux：没有常驻服务，用 systemd 用户级 timer 定时拉起 `--scheduled-round`。
        // `--install-agent` 在这里等同 `--install-service`，免得脚本要按平台分支。
        #[cfg(target_os = "linux")]
        Command::InstallService | Command::InstallAgent => {
            service_linux::install(app_dir, settings_minutes(app_dir))
        }
        #[cfg(target_os = "linux")]
        Command::UninstallService | Command::UninstallAgent => service_linux::uninstall(),
        #[cfg(target_os = "linux")]
        Command::InstallShortcut => shortcut_linux::install(app_dir),
        #[cfg(target_os = "linux")]
        Command::UninstallShortcut => shortcut_linux::uninstall(),
        #[cfg(target_os = "linux")]
        Command::RunService => {
            let _ = app_dir;
            bail!(
                "Linux 没有常驻服务进程：后台搬运由 systemd 用户级 timer 调度，请用 --scheduled-round"
            )
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        Command::InstallService
        | Command::UninstallService
        | Command::InstallAgent
        | Command::UninstallAgent
        | Command::InstallShortcut
        | Command::UninstallShortcut
        | Command::RunService => {
            let _ = app_dir;
            bail!("后台服务与快捷方式目前只在 Windows / macOS / Linux 上提供")
        }
    }
}

fn service_status_text(app_dir: &Path) -> Result<String> {
    Ok(format!(
        "{}\n{}\n{}",
        config_dir_line(app_dir),
        service_status_body(app_dir)?,
        rime_dir_line(app_dir)
    ))
}

/// 配置/日志目录，以及它是否从程序目录回退过来的。
///
/// 装在 `/opt`、`/usr/local/bin` 这类 root 所有的目录时，普通用户写不了
/// 「指定 RIME 用户词库」的结果，看起来就像「怎么也识别不到词库」，
/// 所以这一行要能直接看出配置到底落在哪。
fn config_dir_line(app_dir: &Path) -> String {
    match config::app_dir_note() {
        Some(note) => format!("配置目录：{}（{note}）", app_dir.display()),
        None => format!("配置目录：{}（程序所在目录）", app_dir.display()),
    }
}

/// 汇报程序当前认为的 RIME 用户目录。
///
/// 「找不到词库」几乎都出在目录判断上（以 root / sudo 运行时 `HOME` 指向 `/root`，
/// 或 fcitx5 与 ibus 两套目录并存），所以把来源和可用性一起打出来。
fn rime_dir_line(app_dir: &Path) -> String {
    // 只读：配置文件不存在时不要顺手创建，`--service-status` 不该有副作用。
    let ini = app_dir.join("RimeUserDictSync.ini");
    let settings = if ini.is_file() {
        config::Settings::load(&ini).ok()
    } else {
        None
    };
    let configured = settings
        .as_ref()
        .is_some_and(|settings| !settings.user_data_dir.trim().is_empty());
    let dir = match &settings {
        Some(settings) => crate::context::resolve_rime_dir(settings),
        None => crate::platform::default_user_dir(),
    };
    let usable = dir.join("installation.yaml").is_file();
    let state = if usable {
        "可用".to_owned()
    } else {
        "缺少 installation.yaml".to_owned()
    };
    let source = if configured {
        "配置指定"
    } else {
        "自动探测"
    };
    let mut line = format!("RIME 用户目录：{}（{source}，{state}）", dir.display());
    if !usable {
        // 不可用时把扫过的候选一并列出，否则用户除了「不存在」拿不到任何线索。
        line.push_str(&crate::platform::rime_dir_diagnostics());
    }
    line
}

fn service_status_body(app_dir: &Path) -> Result<String> {
    #[cfg(target_os = "windows")]
    {
        Ok(format!(
            "{}\n{}",
            service::describe(),
            shortcut::describe(app_dir)
        ))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(format!(
            "{}\n{}",
            agent::describe(),
            shortcut_macos::describe(app_dir)
        ))
    }
    #[cfg(target_os = "linux")]
    {
        // Linux 的快捷方式和配置目录无关（固定在 XDG 应用菜单目录），这里只为对齐签名。
        let _ = app_dir;
        Ok(format!(
            "{}\n{}",
            service_linux::describe(),
            shortcut_linux::describe()
        ))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = app_dir;
        Ok("本平台暂不支持后台服务与快捷方式：后台搬运由程序在运行期间执行。".to_owned())
    }
}

/// `--install-service` / `--install-agent` 的间隔：沿用「后台搬运」设置里的分钟数。
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn settings_minutes(app_dir: &Path) -> u32 {
    config::Settings::load(&app_dir.join("RimeUserDictSync.ini"))
        .map(|settings| settings.background_minutes)
        .unwrap_or(10)
}

fn log_line(path: &Path, message: &str) {
    daemon::append_log(path, &report::timestamped(message));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_dir_rejects_missing_empty_and_whitespace_arguments() {
        for arguments in [
            vec!["--app-dir".to_owned()],
            vec!["--app-dir".to_owned(), String::new()],
            vec!["--app-dir".to_owned(), " \t ".to_owned()],
        ] {
            assert!(parse(&arguments).is_err());
        }
    }

    #[test]
    fn app_dir_keeps_valid_paths_with_spaces() {
        let options = parse(&["--app-dir".to_owned(), "a directory".to_owned()]).unwrap();
        assert_eq!(options.app_dir, PathBuf::from("a directory"));
    }
}
