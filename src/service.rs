//! Windows 后台服务：只做 §3 的纯搬运，不调用 `/sync`、`/deploy`。
//!
//! 为什么服务可以只做搬运：`/sync` 与 `/deploy` 必须在用户的登录会话里操作输入法进程，
//! 而搬运只是文件与 WebDAV 的读写，跑在会话 0 完全没有问题。
//!
//! 服务以 `LocalSystem` 运行，因此**不能依赖 `%APPDATA%` 之类的用户环境变量**：
//! 目标 RIME 目录由 GUI 在安装时以 `--app-dir` 参数固定下来。

#[cfg(target_os = "windows")]
mod imp {
    use crate::{config::Settings, daemon, report::Reporter};
    use anyhow::{Context as _, Result, bail};
    use std::{
        ffi::{OsStr, OsString},
        fs,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::Duration,
    };
    use windows_service::{
        define_windows_service,
        service::{
            ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
            ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
            ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };
    use windows_sys::Win32::Foundation::ERROR_SERVICE_DOES_NOT_EXIST;

    pub const SERVICE_NAME: &str = "RimeUserDictSync";
    pub const SERVICE_DISPLAY_NAME: &str = "RIME 用户词库同步后台搬运";
    pub const SERVICE_DESCRIPTION: &str = "在后台定期把 RIME 用户词库搬运到 WebDAV。只搬运文件，不调用 /sync 或 /deploy，因此不会打断输入法。";

    const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
    /// 配置目录开关。安装时也会写进服务的 `lpBinaryPathName`，但**别指望它**：
    /// SCM 只会把 `StartService` 的参数传给 `ServiceMain`，`lpBinaryPathName` 里
    /// 那些参数不会出现（实测 `ServiceMain` 只收到服务名本身）。
    /// 所以真正靠得住的是下面两个落盘记录。
    pub const APP_DIR_ARG: &str = "--app-dir";
    /// 安装时写在服务目录里的配置目录记录，注册表读不到时的兜底。
    const APP_DIR_FILE: &str = "app-dir.txt";
    const LOG_FILE: &str = "RimeSync.log";

    /// 服务自身副本所在目录（`%ProgramData%\RimeUserDictSync`）。
    pub fn install_dir() -> PathBuf {
        std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join("RimeUserDictSync")
    }

    pub fn installed_exe() -> PathBuf {
        install_dir().join("RimeUserDictSync.exe")
    }

    fn app_dir_file() -> PathBuf {
        install_dir().join(APP_DIR_FILE)
    }

    /// 在 HKLM 里记下安装时使用的配置目录，便于 GUI 校验"服务是否还指着当前目录"。
    fn registry_key() -> Result<winreg::RegKey> {
        let hklm = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE);
        Ok(hklm.create_subkey(r"SOFTWARE\RimeUserDictSync")?.0)
    }

    fn remember_app_dir(app_dir: &Path) -> Result<()> {
        let key = registry_key()?;
        key.set_value("AppDir", &app_dir.to_string_lossy().into_owned())
            .context("写入服务配置目录注册表项失败")?;
        Ok(())
    }

    /// 只读地取回安装时记下的配置目录。
    ///
    /// 这里刻意用 `open_subkey` 而不是 `create_subkey`：GUI 以普通用户身份
    /// 也要能读它来显示"服务指向哪个目录"，而写 HKLM 需要管理员权限。
    fn remembered_app_dir() -> Option<PathBuf> {
        let key = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
            .open_subkey(r"SOFTWARE\RimeUserDictSync")
            .ok()?;
        let value: String = key.get_value("AppDir").ok()?;
        (!value.trim().is_empty()).then(|| PathBuf::from(value))
    }

    /// 从落盘记录里读配置目录；目录必须真实存在，否则当作没有。
    fn app_dir_from_file(path: &Path) -> Option<PathBuf> {
        let text = fs::read_to_string(path).ok()?;
        let dir = PathBuf::from(text.trim());
        dir.is_dir().then_some(dir)
    }

    fn forget_app_dir() {
        if let Ok(hklm) = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
            .open_subkey(r"SOFTWARE\RimeUserDictSync")
        {
            let _ = hklm.delete_value("AppDir");
        }
        let _ = fs::remove_file(app_dir_file());
    }

    // ------------------------------------------------------------------ 状态

    /// 服务当前状态，供 GUI 显示与决策。
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Status {
        NotInstalled,
        Stopped,
        Starting,
        Running,
        Stopping,
        Paused,
        Unknown,
    }

    impl Status {
        pub fn label(&self) -> &'static str {
            match self {
                Self::NotInstalled => "未安装",
                Self::Stopped => "已安装，未运行",
                Self::Starting => "正在启动",
                Self::Running => "运行中",
                Self::Stopping => "正在停止",
                Self::Paused => "已暂停",
                Self::Unknown => "状态未知",
            }
        }
    }

    pub fn status() -> Status {
        let Ok(manager) =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        else {
            return Status::Unknown;
        };
        match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
            Ok(service) => match service.query_status() {
                Ok(status) => match status.current_state {
                    ServiceState::Stopped => Status::Stopped,
                    ServiceState::StartPending => Status::Starting,
                    ServiceState::Running => Status::Running,
                    ServiceState::StopPending => Status::Stopping,
                    ServiceState::Paused => Status::Paused,
                    _ => Status::Unknown,
                },
                Err(_) => Status::Unknown,
            },
            Err(_) => Status::NotInstalled,
        }
    }

    /// 一行描述，交给 UI 直接显示。
    pub fn describe() -> String {
        let status = status();
        if status == Status::NotInstalled {
            return format!(
                "后台服务：{}（不安装时由本程序在运行期间执行后台搬运）",
                status.label()
            );
        }
        match remembered_app_dir() {
            Some(dir) => format!("后台服务：{}　配置目录：{}", status.label(), dir.display()),
            None => format!("后台服务：{}", status.label()),
        }
    }

    // ------------------------------------------------------------------ 安装

    pub fn install(app_dir: &Path) -> Result<String> {
        if !app_dir.is_dir() {
            bail!("配置目录不存在：{}", app_dir.display());
        }
        // 先确认拿得到服务管理器权限：提权失败时不要在 ProgramData 里留下半个安装。
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )
        .context(
            "连接服务管理器失败（安装服务需要管理员权限；请从 GUI 点「安装 / 更新后台服务」，\
             或以管理员身份运行命令行）",
        )?;

        let target = installed_exe();
        fs::create_dir_all(install_dir())
            .with_context(|| format!("创建 {} 失败", install_dir().display()))?;

        let access = ServiceAccess::CHANGE_CONFIG
            | ServiceAccess::START
            | ServiceAccess::STOP
            | ServiceAccess::QUERY_STATUS;

        // 已安装就先把它停掉：运行中的服务占着 %ProgramData% 里的 EXE，
        // 直接覆盖会失败并报"另一个程序正在使用此文件"。
        let existing = match manager.open_service(SERVICE_NAME, access) {
            Ok(service) => Some(service),
            Err(windows_service::Error::Winapi(error))
                if error.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) =>
            {
                None
            }
            Err(error) => return Err(error).context("打开已有服务失败"),
        };
        let was_running = match &existing {
            Some(service) => {
                let running = service.query_status()?.current_state != ServiceState::Stopped;
                if running {
                    let _ = service.stop();
                    wait_until_stopped(service, Duration::from_secs(30));
                }
                running
            }
            None => false,
        };

        let current = std::env::current_exe().context("无法确定程序路径")?;
        if current != target {
            // 服务从固定位置启动，用户移动便携目录不会把它弄坏。
            fs::copy(&current, &target)
                .with_context(|| format!("复制程序到 {} 失败", target.display()))?;
        }

        // 把配置目录同时写进注册表和一份落盘记录：开机自启时
        // ServiceMain 收不到 lpBinaryPathName 里的参数，只能靠这两份记录。
        remember_app_dir(app_dir)?;
        fs::write(app_dir_file(), app_dir.to_string_lossy().as_bytes())
            .with_context(|| format!("写出 {} 失败", app_dir_file().display()))?;

        let info = service_info(&target, app_dir);
        let updated = existing.is_some();
        let service = match existing {
            // 已经装过：原地改配置，保留 SCM 里的其他设置。
            Some(service) => {
                service.change_config(&info).context("更新服务配置失败")?;
                service
            }
            None => manager
                .create_service(&info, access)
                .context("创建 Windows 服务失败")?,
        };
        service
            .set_description(SERVICE_DESCRIPTION)
            .context("设置服务描述失败")?;
        // 延迟自启：不拖慢开机，也避免和用户登录时的磁盘争用撞在一起。
        service
            .set_delayed_auto_start(true)
            .context("设置延迟自启动失败")?;
        service
            .update_failure_actions(ServiceFailureActions {
                reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
                reboot_msg: None,
                command: None,
                actions: Some(vec![
                    ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: Duration::from_secs(60),
                    },
                    ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: Duration::from_secs(300),
                    },
                ]),
            })
            .context("设置服务失败重启策略失败")?;
        let _ = service.set_failure_actions_on_non_crash_failures(true);

        service
            .start::<&OsStr>(&[])
            .context("启动服务失败（可在「服务」管理单元中手动启动）")?;
        Ok(if updated && was_running {
            format!(
                "后台服务已更新并重启（{}），配置目录 {}。",
                target.display(),
                app_dir.display()
            )
        } else if updated {
            format!(
                "后台服务已更新并启动（{}），配置目录 {}。",
                target.display(),
                app_dir.display()
            )
        } else {
            format!(
                "后台服务已安装并启动（{}），配置目录 {}。",
                target.display(),
                app_dir.display()
            )
        })
    }

    /// 等 SCM 把状态切到 Stopped；超时就放弃（`start` 会给出真正的错误）。
    fn wait_until_stopped(service: &windows_service::service::Service, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            match service.query_status() {
                Ok(status) if status.current_state == ServiceState::Stopped => return,
                Ok(_) => std::thread::sleep(Duration::from_millis(250)),
                Err(_) => return,
            }
        }
    }

    fn service_info(executable: &Path, app_dir: &Path) -> ServiceInfo {
        ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from(SERVICE_DISPLAY_NAME),
            service_type: SERVICE_TYPE,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: executable.to_path_buf(),
            launch_arguments: vec![
                OsString::from("--run-service"),
                OsString::from(APP_DIR_ARG),
                app_dir.as_os_str().to_os_string(),
            ],
            dependencies: vec![],
            // None = LocalSystem：不需要存用户密码，改密码也不会让服务起不来。
            account_name: None,
            account_password: None,
        }
    }

    pub fn uninstall() -> Result<String> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .context("连接服务管理器失败（需要管理员权限）")?;
        let access = ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS;
        let mut removed = true;
        match manager.open_service(SERVICE_NAME, access) {
            Ok(service) => {
                service.delete().context("标记删除服务失败")?;
                if service.query_status()?.current_state != ServiceState::Stopped {
                    let _ = service.stop();
                }
                drop(service);
                // Win32 没有"等待删除完成"的接口，只能自己轮询。
                for _ in 0..20 {
                    if manager
                        .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)
                        .is_err()
                    {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
            Err(windows_service::Error::Winapi(error))
                if error.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) =>
            {
                removed = false;
            }
            Err(error) => return Err(error).context("打开服务失败"),
        }
        forget_app_dir();
        // 服务副本可能仍被 SCM 句柄占用，删不掉也不算失败。
        let _ = fs::remove_dir_all(install_dir());
        Ok(if removed {
            "后台服务已卸载。".to_owned()
        } else {
            "后台服务未安装，无需卸载。".to_owned()
        })
    }

    /// 以管理员身份重新启动自己（UAC 提权），用于安装/卸载服务。
    pub fn request_elevated(arguments: &[String]) -> Result<()> {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let exe = std::env::current_exe().context("无法确定程序路径")?;
        let parameters = arguments
            .iter()
            .map(|argument| crate::platform::quote_windows_argument(argument))
            .collect::<Vec<_>>()
            .join(" ");
        let wide = |text: &str| {
            text.encode_utf16()
                .chain(std::iter::once(0))
                .collect::<Vec<u16>>()
        };
        let verb = wide("runas");
        let file = wide(&exe.to_string_lossy());
        let parameters = wide(&parameters);
        // HWND/HINSTANCE 在 windows-sys 0.52 里是 isize。
        let result = unsafe {
            ShellExecuteW(
                0,
                verb.as_ptr(),
                file.as_ptr(),
                parameters.as_ptr(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        if result <= 32 {
            bail!(
                "请求管理员权限失败（ShellExecute 返回 {result}）；可以右键以管理员身份运行程序，\
                 或手动执行 `RimeUserDictSync.exe --install-service`。"
            );
        }
        Ok(())
    }

    // -------------------------------------------------------------- 服务本体

    pub fn run_dispatcher() -> Result<()> {
        service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|error| {
            anyhow::anyhow!(
                "{error}（--run-service 只能由服务控制管理器启动；\
                 手工测试请改用 --round）"
            )
        })
    }

    define_windows_service!(ffi_service_main, service_main);

    fn service_main(arguments: Vec<OsString>) {
        let (app_dir, warning) = resolve_app_dir(&arguments);
        // 先把启动参数原样落盘：万一配置目录没解析对，这里能一眼看出来。
        daemon::append_log(
            &app_dir.join(LOG_FILE),
            &crate::report::timestamped(&format!(
                "服务启动参数：{}",
                arguments
                    .iter()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            )),
        );
        if let Some(warning) = warning {
            daemon::append_log(
                &app_dir.join(LOG_FILE),
                &crate::report::timestamped(&format!("警告：{warning}")),
            );
        }
        if let Err(error) = run_service(&app_dir) {
            // 服务没有控制台，出错只能落盘。
            daemon::append_log(
                &app_dir.join(LOG_FILE),
                &crate::report::timestamped(&format!("服务异常退出: {error:#}")),
            );
        }
    }

    /// 解析配置目录。**不能只靠启动参数**：SCM 只把 `StartService` 的参数传给
    /// `ServiceMain`，`lpBinaryPathName` 里的 `--app-dir` 根本不会出现，
    /// 所以开机自启时必须能从落盘记录里读回来。
    ///
    /// 顺序：启动参数 → HKLM 注册表 → 服务目录下的 app-dir.txt → 程序目录（并告警）。
    fn resolve_app_dir(arguments: &[OsString]) -> (PathBuf, Option<String>) {
        let mut arguments = arguments.iter();
        while let Some(argument) = arguments.next() {
            if argument == APP_DIR_ARG
                && let Some(value) = arguments.next()
            {
                return (PathBuf::from(value), None);
            }
        }
        if let Some(dir) = remembered_app_dir().filter(|dir| dir.is_dir()) {
            return (dir, None);
        }
        if let Some(dir) = app_dir_from_file(&app_dir_file()) {
            return (dir, None);
        }
        // 兜底：服务自身副本所在目录。这几乎总是错的（那是 %ProgramData%，
        // 而不是 GUI 的配置目录），所以返回一条明确的告警。
        let fallback = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        (
            fallback,
            Some(format!(
                "启动参数、注册表与 {} 里都没有可用的配置目录，已退回使用程序目录。\
                 请重新执行一次 --install-service --app-dir <配置目录>。",
                app_dir_file().display()
            )),
        )
    }

    fn service_status(state: ServiceState, accepted: ServiceControlAccept) -> ServiceStatus {
        ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: state,
            controls_accepted: accepted,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(30),
            process_id: None,
        }
    }

    fn run_service(app_dir: &Path) -> windows_service::Result<()> {
        let ini = app_dir.join("RimeUserDictSync.ini");
        let reporter = daemon::FileReporter::new(app_dir);
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<()>();

        let handler = {
            let stop = stop.clone();
            let cancel = cancel.clone();
            move |event: ServiceControl| -> ServiceControlHandlerResult {
                match event {
                    ServiceControl::Stop | ServiceControl::Shutdown => {
                        stop.store(true, Ordering::Relaxed);
                        // 让正在跑的一轮尽快在下一个检查点退出。
                        cancel.store(true, Ordering::Relaxed);
                        let _ = tx.send(());
                        ServiceControlHandlerResult::NoError
                    }
                    ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                    _ => ServiceControlHandlerResult::NotImplemented,
                }
            }
        };
        let status_handle = service_control_handler::register(SERVICE_NAME, handler)?;
        status_handle.set_service_status(service_status(
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        ))?;
        reporter.log(&format!(
            "服务已启动：配置目录 {}，日志 {}。",
            app_dir.display(),
            app_dir.join(LOG_FILE).display()
        ));

        let mut due = daemon::DueAt::now();
        while !stop.load(Ordering::Relaxed) {
            // 每 TICK 醒一次：Stop 会被控制处理器立刻唤醒，休眠唤醒后也会立刻补跑。
            match rx.recv_timeout(daemon::TICK) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let settings = match Settings::load(&ini) {
                Ok(settings) => settings,
                Err(error) => {
                    reporter.log(&format!("读取配置失败，本轮跳过: {error}"));
                    continue;
                }
            };
            let schedule = daemon::Schedule::from_settings(&settings);
            let wake = daemon::take_wake_for_settings(&settings);
            if !wake && (!schedule.enabled || !due.is_due()) {
                continue;
            }
            due.reschedule(schedule.interval);
            cancel.store(false, Ordering::Relaxed);
            // 每轮都留下"为什么跑"的痕迹，便于对照日志判断服务是否真的在工作。
            reporter.log(&format!(
                "{}，开始一轮后台搬运（间隔 {} 分钟）。",
                if wake {
                    "收到唤醒请求"
                } else {
                    "定时到期"
                },
                settings.background_minutes
            ));
            match daemon::run_once(app_dir, &ini, &cancel, &reporter) {
                Ok(message) => reporter.log(&message),
                Err(error) => reporter.log(&format!("后台搬运未完成: {error:#}")),
            }
        }

        status_handle.set_service_status(service_status(
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
        ))?;
        reporter.log("服务已停止。");
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn app_dir_file_round_trips() {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("app-dir.txt");
            fs::write(&file, format!("{}\n", dir.path().display())).unwrap();
            assert_eq!(
                app_dir_from_file(&file),
                Some(dir.path().to_path_buf()),
                "带换行的记录也要能读回来"
            );
        }

        #[test]
        fn app_dir_file_ignores_missing_directory() {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("app-dir.txt");
            fs::write(&file, r"D:\definitely\not\here").unwrap();
            assert_eq!(app_dir_from_file(&file), None);
            assert_eq!(app_dir_from_file(&dir.path().join("nope.txt")), None);
        }

        /// 服务启动参数里解析出的 `--app-dir` 优先于任何落盘记录。
        #[test]
        fn explicit_argument_wins() {
            let dir = tempfile::tempdir().unwrap();
            let arguments = vec![
                OsString::from(SERVICE_NAME),
                OsString::from("--run-service"),
                OsString::from(APP_DIR_ARG),
                dir.path().as_os_str().to_os_string(),
            ];
            let (resolved, warning) = resolve_app_dir(&arguments);
            assert_eq!(resolved, dir.path());
            assert!(warning.is_none());
        }
    }
}

#[cfg(target_os = "windows")]
pub use imp::*;
