use anyhow::{Context, Result, bail};
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// RIME 命令的最长执行时间。超时就杀掉：进程卡住会一直占着同步锁，
/// 而后台轮次没有「停止同步」按钮，只能重启程序才能恢复。
const RIME_TIMEOUT: Duration = Duration::from_secs(300);

/// RIME 两步动作：合并 userdb 快照 / 重新部署。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RimeStage {
    Sync,
    Deploy,
}

/// 一条 RIME 命令。
///
/// 这里存**完整 argv** 而不是「一个可执行文件 + 一个固定参数」，因为 Linux 上
/// 两步是两个不同的程序：`rime_dict_manager --sync` 合并 userdb，
/// `rime_deployer --build <user> <shared>` 重新部署，两者都不像
/// `WeaselDeployer.exe /sync` 那样共用一个可执行文件。
pub struct RimeCommand {
    pub sync: Vec<String>,
    pub deploy: Vec<String>,
    /// 部署之后尽力让输入法前端重新加载（`fcitx5-remote -r`）。
    /// 失败不算同步失败：数据已经落到 `build/`，切换输入法时自然会用上。
    pub reload: Vec<String>,
    /// 子进程的工作目录。
    ///
    /// librime 的命令行工具从**当前目录**读 `installation.yaml`，并据此确定
    /// `installation_id` 与 `sync_dir`，所以必须把 cwd 固定到 RIME 用户目录；
    /// 否则它会去 `~/.config/ibus/rime` 之类的位置找，读到空配置后静默不合并。
    pub cwd: Option<PathBuf>,
}

impl RimeCommand {
    fn argv(&self, stage: RimeStage) -> &[String] {
        match stage {
            RimeStage::Sync => &self.sync,
            RimeStage::Deploy => &self.deploy,
        }
    }

    /// 供日志与报错使用的可读命令行。
    pub fn describe(&self, stage: RimeStage) -> String {
        self.argv(stage).join(" ")
    }
}

pub fn default_user_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        use winreg::{RegKey, enums::*};
        let current_user = RegKey::predef(HKEY_CURRENT_USER);
        for flags in [KEY_READ | KEY_WOW64_64KEY, KEY_READ | KEY_WOW64_32KEY] {
            if let Ok(key) = current_user.open_subkey_with_flags("SOFTWARE\\Rime\\Weasel", flags)
                && let Ok::<String, _>(dir) = key.get_value("RimeUserDir")
                && !dir.trim().is_empty()
            {
                return PathBuf::from(expand_env(&dir));
            }
        }
        return std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("Rime");
    }
    #[cfg(target_os = "macos")]
    {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("Library/Rime");
    }
    #[cfg(target_os = "linux")]
    {
        // Linux 没有统一的 RIME 用户目录：fcitx5-rime 与 ibus-rime 各用一套。
        // 判定顺序：含 installation.yaml 的优先（这才是真正在用的那套），
        // 其次已存在的目录，都没有才退回 fcitx5 的默认位置。
        let candidates = linux_rime_candidates();
        if let Some(found) = candidates
            .iter()
            .find(|dir| dir.join("installation.yaml").is_file())
        {
            return found.clone();
        }
        if let Some(found) = candidates.iter().find(|dir| dir.is_dir()) {
            return found.clone();
        }
        return candidates.into_iter().next().unwrap_or_default();
    }
    #[allow(unreachable_code)]
    PathBuf::new()
}

/// Linux 上 RIME 用户目录的候选位置，按「更可能是当前在用的」排序。
#[cfg(target_os = "linux")]
fn linux_rime_candidates() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".local/share")));
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".config")));

    let mut candidates = Vec::new();
    // fcitx5-rime：XDG 数据目录下的 fcitx5/rime。
    if let Some(data_home) = &data_home {
        candidates.push(data_home.join("fcitx5/rime"));
    }
    // ibus-rime：XDG 配置目录下的 ibus/rime。
    if let Some(config_home) = &config_home {
        candidates.push(config_home.join("ibus/rime"));
        // 少数老发行版把 fcitx5 的 RIME 数据放在 ~/.config/fcitx5/rime。
        candidates.push(config_home.join("fcitx5/rime"));
    }
    candidates
}

/// librime 的共享数据目录（方案与预编译产物都在这里）。
#[cfg(target_os = "linux")]
fn linux_shared_data_dir() -> Option<PathBuf> {
    ["/usr/share/rime-data", "/usr/local/share/rime-data"]
        .into_iter()
        .map(PathBuf::from)
        .find(|dir| dir.is_dir())
}

/// 在 PATH 里找一个可执行文件；找不到再看 `/usr/bin`。
#[cfg(target_os = "linux")]
fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let is_executable = |path: &Path| {
        path.metadata()
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    let found = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(name))
                .find(|candidate| is_executable(candidate))
        })
        .unwrap_or_default();
    found.or_else(|| {
        let fallback = PathBuf::from("/usr/bin").join(name);
        is_executable(&fallback).then_some(fallback)
    })
}

/// 把路径转成 argv 里的字符串。
fn argument(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub fn find_rime(configured: &str, app_dir: &Path, rime_dir: &Path) -> Result<RimeCommand> {
    #[cfg(not(target_os = "windows"))]
    let _ = app_dir;
    #[cfg(not(target_os = "linux"))]
    let _ = rime_dir;
    #[cfg(target_os = "windows")]
    {
        let mut candidates = Vec::new();
        if !configured.trim().is_empty() {
            candidates.push(PathBuf::from(expand_env(configured)));
        }
        candidates.push(app_dir.join("WeaselDeployer.exe"));
        use winreg::{RegKey, enums::*};
        for hive in [
            RegKey::predef(HKEY_LOCAL_MACHINE),
            RegKey::predef(HKEY_CURRENT_USER),
        ] {
            for flags in [KEY_READ | KEY_WOW64_64KEY, KEY_READ | KEY_WOW64_32KEY] {
                if let Ok(key) = hive.open_subkey_with_flags("SOFTWARE\\Rime\\Weasel", flags) {
                    if let Ok::<String, _>(dir) = key.get_value("WeaselRoot") {
                        candidates.push(PathBuf::from(expand_env(&dir)).join("WeaselDeployer.exe"));
                    }
                    if let Ok::<String, _>(dir) = key.get_value("InstallDir") {
                        add_install_dir_candidates(&mut candidates, Path::new(&expand_env(&dir)));
                    }
                }
            }
        }
        for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(dir) = std::env::var_os(variable) {
                add_install_dir_candidates(&mut candidates, &PathBuf::from(dir).join("Rime"));
            }
        }
        let executable = candidates
            .into_iter()
            .find(|p| p.is_file())
            .context("找不到 WeaselDeployer.exe，请在配置中指定路径")?;
        let executable = argument(&executable);
        Ok(RimeCommand {
            sync: vec![executable.clone(), "/sync".to_owned()],
            deploy: vec![executable, "/deploy".to_owned()],
            reload: Vec::new(),
            cwd: None,
        })
    }
    #[cfg(target_os = "macos")]
    {
        let executable = if configured.trim().is_empty() {
            PathBuf::from("/Library/Input Methods/Squirrel.app/Contents/MacOS/Squirrel")
        } else {
            PathBuf::from(expand_env(configured))
        };
        if !executable.is_file() {
            bail!("找不到鼠须管 Squirrel: {}", executable.display());
        }
        let executable = argument(&executable);
        Ok(RimeCommand {
            sync: vec![executable.clone(), "--sync".to_owned()],
            deploy: vec![executable, "--reload".to_owned()],
            reload: Vec::new(),
            cwd: None,
        })
    }
    #[cfg(target_os = "linux")]
    {
        let user_dir = if rime_dir.as_os_str().is_empty() {
            default_user_dir()
        } else {
            rime_dir.to_path_buf()
        };
        // 1) 显式配置优先：沿用旧约定，自己指定的工具收 `--sync` / `--deploy`。
        if !configured.trim().is_empty() {
            let executable = PathBuf::from(expand_env(configured));
            if !executable.is_file() {
                bail!("找不到配置指定的 RIME 工具：{}", executable.display());
            }
            let executable = argument(&executable);
            return Ok(RimeCommand {
                sync: vec![executable.clone(), "--sync".to_owned()],
                deploy: vec![executable, "--deploy".to_owned()],
                reload: Vec::new(),
                cwd: Some(user_dir),
            });
        }
        // 2) 自动探测 librime 自带的命令行工具（Debian/Ubuntu 包名 librime-bin）。
        let (Some(dict_manager), Some(deployer)) =
            (which("rime_dict_manager"), which("rime_deployer"))
        else {
            bail!(
                "未找到 librime 的命令行工具（rime_dict_manager / rime_deployer）。\n\
                 请先安装：sudo apt install librime-bin\n\
                 或在 INI 的 [rime] deployer_path 中指定可执行的部署工具。"
            );
        };
        let Some(shared) = linux_shared_data_dir() else {
            bail!(
                "找不到 RIME 共享数据目录（/usr/share/rime-data）。\n\
                 请先安装方案数据：sudo apt install librime-data"
            );
        };
        // 前端各自的重载方式：fcitx5 有 CLI，ibus 只能靠重新选择引擎，这里只做 fcitx5。
        let reload = which("fcitx5-remote")
            .map(|tool| vec![argument(&tool), "-r".to_owned()])
            .unwrap_or_default();
        Ok(RimeCommand {
            // 两个工具都从 cwd 读 installation.yaml（user_id / sync_dir）。
            sync: vec![argument(&dict_manager), "--sync".to_owned()],
            deploy: vec![
                argument(&deployer),
                "--build".to_owned(),
                argument(&user_dir),
                argument(&shared),
            ],
            reload,
            cwd: Some(user_dir),
        })
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    bail!("不支持当前平台")
}

#[cfg(target_os = "windows")]
fn add_install_dir_candidates(candidates: &mut Vec<PathBuf>, root: &Path) {
    candidates.push(root.join("WeaselDeployer.exe"));
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.filter_map(Result::ok) {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                candidates.push(entry.path().join("WeaselDeployer.exe"));
            }
        }
    }
}

pub fn run(command: &RimeCommand, stage: RimeStage, cancel: &AtomicBool) -> Result<()> {
    run_argv(command.argv(stage), command.cwd.as_deref(), cancel)?;
    // 部署完顺手让前端重新加载一次。失败只当作提示：`build/` 已经更新，
    // 用户切换输入法或重启会话时照样会生效，没必要让整轮同步失败。
    if stage == RimeStage::Deploy && !command.reload.is_empty() {
        let never = AtomicBool::new(false);
        let _ = run_argv(&command.reload, command.cwd.as_deref(), &never);
    }
    Ok(())
}

fn run_argv(argv: &[String], cwd: Option<&Path>, cancel: &AtomicBool) -> Result<()> {
    let Some((program, arguments)) = argv.split_first() else {
        bail!("RIME 命令为空");
    };
    let mut builder = Command::new(program);
    builder
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        builder.current_dir(dir);
    }
    let mut child = builder
        .spawn()
        .with_context(|| format!("无法启动 {program}"))?;
    // 单独排空 stderr：librime 的工具把真正的失败原因写在这里（比如 LevelDB 的
    // `IO error: lock ... already held by process`），只留一个退出码根本没法排查。
    // 必须开线程读，否则管道写满 64 KB 后子进程会卡住。
    let collected = Arc::new(Mutex::new(String::new()));
    if let Some(mut pipe) = child.stderr.take() {
        let sink = collected.clone();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while let Ok(read) = pipe.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                if let Ok(mut text) = sink.lock()
                    && text.len() < 8192
                {
                    text.push_str(&String::from_utf8_lossy(&buffer[..read]));
                }
            }
        });
    }
    let started = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            bail!("用户已停止同步");
        }
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            let details = collected
                .lock()
                .map(|text| text.trim().to_owned())
                .unwrap_or_default();
            let mut message = format!(
                "RIME 命令执行失败，退出码 {:?}：{}",
                status.code(),
                argv.join(" ")
            );
            if !details.is_empty() {
                message.push_str(&format!("\n{details}"));
            }
            message.push_str(lock_hint(&details));
            bail!(message);
        }
        if started.elapsed() >= RIME_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "RIME 命令超过 {} 秒仍未结束，已强制终止：{}",
                RIME_TIMEOUT.as_secs(),
                argv.join(" ")
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// LevelDB 单进程锁的额外提示。
///
/// Linux 上没有「输入法自己执行同步」这条路，只能另起 `rime_dict_manager`，
/// 而正在运行的 fcitx5 / ibus 往往正占着 `*.userdb`：报错原文只有一行
/// `IO error: lock ... already held by process`，不解释的话用户完全无从下手。
fn lock_hint(details: &str) -> &'static str {
    let lowered = details.to_lowercase();
    let locked = lowered.contains("already held")
        || lowered.contains("lock")
        || lowered.contains("资源暂时不可用")
        || lowered.contains("resource temporarily unavailable");
    if !locked {
        return "";
    }
    "\n提示：输入法正占用 userdb（LevelDB 只允许一个进程打开）。\n      \
     请先退出输入法再重试：fcitx5 执行 fcitx5-remote -e；ibus 执行 ibus exit。\n      \
     同步完成后重新启动输入法即可。"
}

/// 按 Windows 命令行分隔规则给参数加引号（含空白或引号时才包裹）。
#[cfg(target_os = "windows")]
pub fn quote_windows_argument(argument: &str) -> String {
    if argument.is_empty() {
        return "\"\"".to_owned();
    }
    if !argument.contains([' ', '\t', '"']) {
        return argument.to_owned();
    }
    let mut out = String::with_capacity(argument.len() + 2);
    out.push('"');
    let mut backslashes = 0;
    for character in argument.chars() {
        match character {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                // 引号前的反斜杠要翻倍，再转义引号本身。
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push('\\');
                out.push('"');
            }
            other => {
                backslashes = 0;
                out.push(other);
            }
        }
    }
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

/// 无界面模式的结果提示。
///
/// 桌面快捷方式双击启动时没有控制台，`println!` 会石沉大海：
/// Windows 用消息框，macOS 用 osascript 原生对话框，Linux 用 libnotify 通知，
/// 其余平台退化为 stderr。
pub fn notify(title: &str, message: &str, error: bool) {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MessageBoxW,
        };
        let wide = |text: &str| {
            text.encode_utf16()
                .chain(std::iter::once(0))
                .collect::<Vec<u16>>()
        };
        let text = wide(message);
        let caption = wide(title);
        let style = MB_OK
            | MB_SETFOREGROUND
            | if error {
                MB_ICONERROR
            } else {
                MB_ICONINFORMATION
            };
        // HWND 在 windows-sys 0.52 里是 isize，0 表示无父窗口。
        unsafe { MessageBoxW(0, text.as_ptr(), caption.as_ptr(), style) };
    }
    // macOS 没有消息框 API，用 osascript 弹一个原生对话框；
    // 快捷方式（AppleScript applet）双击时没有终端，这是唯一能让用户看到结果的方式。
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display dialog \"{}\" buttons {{\"好\"}} default button 1 with title \"{}\" with icon {}",
            applescript_escape(message),
            applescript_escape(title),
            if error { "stop" } else { "note" }
        );
        let _ = Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(script)
            .status();
    }
    // Linux 用桌面通知（notify-send 来自 libnotify-bin，Mint/Ubuntu 默认自带）。
    // 之前这里只往 stderr 打印，而桌面菜单启动的进程根本没有终端，等于什么都没提示。
    #[cfg(target_os = "linux")]
    {
        let urgency = if error { "critical" } else { "normal" };
        let icon = if error {
            "dialog-error"
        } else {
            "dialog-information"
        };
        let shown = Command::new("notify-send")
            .args(["-a", "RIME 用户词库同步", "-u", urgency, "-i", icon])
            .arg(title)
            .arg(message)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !shown {
            eprintln!("{title}: {message}");
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = error;
        eprintln!("{title}: {message}");
    }
}

/// AppleScript 字符串字面量转义（反斜杠、双引号、换行）。
#[cfg(target_os = "macos")]
fn applescript_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\r', '\n'], "\\n")
}

/// 展开 %VAR% 形式的环境变量（注册表 REG_EXPAND_SZ 与旧版 INI 值可能包含）。
pub fn expand_env(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('%') {
        let after = &rest[start + 1..];
        let expanded = after.find('%').and_then(|end| {
            let name = &after[..end];
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                std::env::var(name).ok().map(|var| (end, var))
            } else {
                None
            }
        });
        out.push_str(&rest[..start]);
        if let Some((end, var)) = expanded {
            out.push_str(&var);
            rest = &after[end + 1..];
        } else {
            out.push('%');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leveldb_lock_errors_get_an_actionable_hint() {
        // librime 的原文，Linux 上几乎必然遇到（fcitx5 占着 userdb 时）。
        let stderr = "IO error: lock /home/me/.local/share/fcitx5/rime/rime.userdb/LOCK: \
                      already held by process";
        assert!(lock_hint(stderr).contains("fcitx5-remote -e"));
        assert!(lock_hint("Resource temporarily unavailable").contains("ibus exit"));
        assert_eq!(lock_hint("permission denied"), "");
        assert_eq!(lock_hint(""), "");
    }

    #[test]
    fn rime_command_keeps_sync_and_deploy_apart() {
        // Linux 上这两步是两个不同的程序，不能退化成「同一个 exe 两个参数」。
        let command = RimeCommand {
            sync: vec!["rime_dict_manager".to_owned(), "--sync".to_owned()],
            deploy: vec![
                "rime_deployer".to_owned(),
                "--build".to_owned(),
                "/home/me/rime".to_owned(),
            ],
            reload: Vec::new(),
            cwd: Some(PathBuf::from("/home/me/rime")),
        };
        assert_eq!(command.describe(RimeStage::Sync), "rime_dict_manager --sync");
        assert_eq!(
            command.describe(RimeStage::Deploy),
            "rime_deployer --build /home/me/rime"
        );
    }

    #[test]
    fn expand_env_expands_known_variables_and_keeps_unknown() {
        let path = std::env::var("PATH").unwrap_or_default();
        assert!(!path.is_empty());
        assert_eq!(expand_env("x%PATH%y"), format!("x{path}y"));
        assert_eq!(
            expand_env("%NO_SUCH_VARIABLE_XYZ%"),
            "%NO_SUCH_VARIABLE_XYZ%"
        );
        assert_eq!(expand_env("100%"), "100%");
        assert_eq!(expand_env("%%"), "%%");
    }
}
