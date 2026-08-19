use anyhow::{Context, Result, bail};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

pub struct RimeCommand {
    pub executable: PathBuf,
    pub sync_arg: &'static str,
    pub deploy_arg: &'static str,
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
                return PathBuf::from(dir);
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
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".local/share/fcitx5/rime");
    }
    #[allow(unreachable_code)]
    PathBuf::new()
}

pub fn find_rime(configured: &str, app_dir: &Path) -> Result<RimeCommand> {
    #[cfg(target_os = "windows")]
    {
        let mut candidates = Vec::new();
        if !configured.trim().is_empty() {
            candidates.push(PathBuf::from(configured));
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
                        candidates.push(PathBuf::from(dir).join("WeaselDeployer.exe"));
                    }
                    if let Ok::<String, _>(dir) = key.get_value("InstallDir") {
                        add_install_dir_candidates(&mut candidates, Path::new(&dir));
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
        Ok(RimeCommand {
            executable,
            sync_arg: "/sync",
            deploy_arg: "/deploy",
        })
    }
    #[cfg(target_os = "macos")]
    {
        let executable = if configured.trim().is_empty() {
            PathBuf::from("/Library/Input Methods/Squirrel.app/Contents/MacOS/Squirrel")
        } else {
            PathBuf::from(configured)
        };
        if !executable.is_file() {
            bail!("找不到鼠须管 Squirrel: {}", executable.display());
        }
        return Ok(RimeCommand {
            executable,
            sync_arg: "--sync",
            deploy_arg: "--reload",
        });
    }
    #[cfg(target_os = "linux")]
    {
        let executable = PathBuf::from(configured);
        if executable.as_os_str().is_empty() || !executable.is_file() {
            bail!("Linux 需要在配置中指定可执行的 RIME 部署工具；不同前端的命令并不统一");
        }
        return Ok(RimeCommand {
            executable,
            sync_arg: "--sync",
            deploy_arg: "--deploy",
        });
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

pub fn run(command: &RimeCommand, arg: &str, cancel: &AtomicBool) -> Result<()> {
    let mut child = Command::new(&command.executable)
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("无法启动 {}", command.executable.display()))?;
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            bail!("用户已停止同步");
        }
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            bail!("RIME 命令执行失败，退出码 {:?}", status.code());
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}
