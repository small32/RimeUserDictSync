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
mod platform;
mod report;
/// 后台服务只在 Windows 上提供；桌面快捷方式两端都有（Windows `.lnk` / macOS `.app`）。
#[cfg(target_os = "windows")]
mod service;
#[cfg(target_os = "windows")]
mod shortcut;
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
                         → RIME /sync → /deploy（桌面快捷方式用的就是它）
  --round                只执行一轮后台搬运后退出（不合并、不部署）
  --scheduled-round      供 macOS 后台代理调用：后台搬运关闭时跳过，否则跑一轮
  --install-service      安装并启动 Windows 后台服务（需要管理员权限）
  --uninstall-service    停止并卸载 Windows 后台服务（需要管理员权限）
  --install-agent        安装 macOS 后台代理（launchd 定时搬运，不需要程序常开）
  --uninstall-agent      卸载 macOS 后台代理
  --install-shortcut     在程序所在目录创建「同步词库」快捷方式
                         （Windows 为 .lnk，macOS 为 AppleScript .app）
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
            let text = format!("失败: {error:#}");
            log_line(&log, &text);
            eprintln!("{text}");
            if options.notify {
                platform::notify(NOTIFY_TITLE, &text, true);
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
        Command::Round => {
            let reporter = daemon::FileReporter::new(app_dir);
            let cancel = AtomicBool::new(false);
            daemon::run_once(app_dir, &ini, &cancel, &reporter)
        }
        // launchd 定时触发。关掉「后台搬运」开关后直接跳过，不产生网络请求与日志。
        Command::ScheduledRound => {
            let settings = config::Settings::load(&ini)?;
            if !settings.background_enabled {
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
        #[cfg(not(target_os = "macos"))]
        Command::InstallAgent | Command::UninstallAgent => {
            let _ = app_dir;
            bail!("后台代理只在 macOS 上提供；Windows 请使用 --install-service")
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        Command::InstallService
        | Command::UninstallService
        | Command::InstallShortcut
        | Command::UninstallShortcut
        | Command::RunService => {
            let _ = app_dir;
            bail!("后台服务与快捷方式目前只在 Windows / macOS 上提供")
        }
    }
}

fn service_status_text(app_dir: &Path) -> Result<String> {
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
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = app_dir;
        Ok("本平台暂不支持后台服务与快捷方式：后台搬运由程序在运行期间执行。".to_owned())
    }
}

/// `--install-agent` 的间隔：沿用「后台搬运」设置里的分钟数。
#[cfg(target_os = "macos")]
fn settings_minutes(app_dir: &Path) -> u32 {
    config::Settings::load(&app_dir.join("RimeUserDictSync.ini"))
        .map(|settings| settings.background_minutes)
        .unwrap_or(10)
}

fn log_line(path: &Path, message: &str) {
    daemon::append_log(path, &report::timestamped(message));
}
