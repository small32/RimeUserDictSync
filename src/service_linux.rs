//! Linux 的「Rime词库同步」：systemd 用户级 timer。
//!
//! 对应关系：
//!
//! | 平台 | 后台搬运 |
//! | --- | --- |
//! | Windows | SCM 服务（`service.rs`，LocalSystem，登录前也跑） |
//! | macOS | launchd LaunchAgent（`agent.rs`，登录后按 `StartInterval` 拉起） |
//! | Linux | systemd **用户级** timer（本文件，登录后按 `OnUnitActiveSec` 拉起） |
//!
//! 三者都是「到点拉起一次 `--scheduled-round`，跑完即退」，没有常驻进程，
//! 也不碰 RIME 根目录下的源文件，所以不打断输入。
//!
//! 为什么用用户级（`--user`）而不是系统级：`--scheduled-round` 要读该用户的
//! `RimeUserDictSync.ini` 与 `RIME/.rime-sync/device.json`，系统级服务跑在
//! 独立账号下根本读不到；而 `/sync`、`/deploy` 那种需要登录会话的动作本来
//! 就只由 GUI 的「同步词库」按钮触发，不需要提权。
//!
//! 需要开机就同步（不登录）时，用户自己开 lingering：
//! `loginctl enable-linger "$USER"`。

use anyhow::{Context as _, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

pub const SERVICE_NAME: &str = "rime-user-dict-sync.service";
pub const TIMER_NAME: &str = "rime-user-dict-sync.timer";
pub const UNIT_TITLE: &str = "Rime词库同步";

fn unit_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let home =
                std::env::var_os("HOME").context("读不到 HOME，无法定位 systemd 用户单元")?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("systemd/user"))
}

fn service_path() -> Result<PathBuf> {
    Ok(unit_dir()?.join(SERVICE_NAME))
}

fn timer_path() -> Result<PathBuf> {
    Ok(unit_dir()?.join(TIMER_NAME))
}

/// 单元文件存在即视为「已开启」。
pub fn installed() -> bool {
    timer_path().map(|path| path.is_file()).unwrap_or(false)
}

/// timer 是否在跑（GUI 内建线程据此让位）。`status()` 里已经包含这个判断。
fn timer_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", TIMER_NAME])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// 状态描述，交给 UI 直接显示。
pub fn describe() -> String {
    status().0
}

/// 一次查询同时拿到「描述文本」与「timer 是否在跑」。
pub fn status() -> (String, bool) {
    let Ok(path) = timer_path() else {
        return (format!("{UNIT_TITLE}：未开启"), false);
    };
    if !path.is_file() {
        return (format!("{UNIT_TITLE}：未开启"), false);
    }
    let minutes = fs::read_to_string(&path)
        .ok()
        .and_then(|text| interval_minutes_of_unit(&text))
        .unwrap_or(0);
    let active = timer_active();
    let mut text = format!("{UNIT_TITLE}：已开启（每 {minutes} 分钟搬运一轮）");
    if !active {
        text.push_str("；timer 未在运行");
    }
    (text, active)
}

/// 安装 / 更新 systemd 用户级 timer。
pub fn install(app_dir: &Path, minutes: u32) -> Result<String> {
    let exe = std::env::current_exe().context("无法确定程序路径")?;
    if !exe.is_file() {
        bail!("程序路径不存在：{}", exe.display());
    }
    let service = service_path()?;
    let timer = timer_path()?;
    if let Some(parent) = service.parent() {
        fs::create_dir_all(parent).context("创建 systemd 用户单元目录失败")?;
    }

    write_atomic(&service, &service_unit(&exe, app_dir))?;
    write_atomic(&timer, &timer_unit(minutes))?;

    systemctl_checked(&["daemon-reload"])?;
    // `--now` 立刻启动；重复执行等于原地更新，不会报错。
    let output = systemctl(&["enable", "--now", TIMER_NAME])?;
    if !output.status.success() {
        bail!(
            "写入单元成功，但 `systemctl --user enable --now {TIMER_NAME}` 失败：{}\n\
             单元文件已保留在 {}；可在图形会话的终端里手动执行该命令。",
            String::from_utf8_lossy(&output.stderr).trim(),
            timer.display()
        );
    }
    Ok(format!(
        "已开启 Rime词库同步：\n\
         · 后台搬运：每 {minutes} 分钟一轮（登录后由 systemd 用户级 timer 拉起，程序不必常开）\n\
         · 单元文件：{}、{}\n\
         · 想在未登录时也同步，请执行：loginctl enable-linger \"$USER\"",
        service.display(),
        timer.display()
    ))
}

/// 停止并移除 timer 与 service。
pub fn uninstall() -> Result<String> {
    let service = service_path()?;
    let timer = timer_path()?;
    if !service.is_file() && !timer.is_file() {
        return Ok(format!("没有开启 {UNIT_TITLE}，无需关闭。"));
    }
    // 先停再删：disable 会顺带移除 timers.target 里的符号链接。
    // 失败不能静默：文件删掉、`installed()` 变 false、界面显示「已移除」，
    // 但已载入内存的 timer 仍在跑，ExecStart 指向的二进制也还在。
    let mut problems: Vec<String> = Vec::new();
    for arguments in [
        &["disable", "--now", TIMER_NAME][..],
        &["stop", SERVICE_NAME][..],
    ] {
        if let Err(error) = systemctl_checked(arguments) {
            problems.push(error.to_string());
        }
    }
    for path in [&timer, &service] {
        if path.is_file() {
            fs::remove_file(path).with_context(|| format!("删除 {} 失败", path.display()))?;
        }
    }
    let _ = systemctl(&["daemon-reload"]);
    if !problems.is_empty() {
        bail!(
            "单元文件已删除，但 systemd 仍在运行旧任务（{}）。\n\
             请在图形会话的终端里执行：systemctl --user disable --now {TIMER_NAME}",
            problems.join("；")
        );
    }
    Ok(format!(
        "已关闭 {UNIT_TITLE}：后台搬运的 timer 与 service 均已移除。"
    ))
}

/// 改间隔：只重写 timer，内容没变就不折腾 systemd。
pub fn refresh_interval(minutes: u32) -> Result<bool> {
    let Ok(path) = timer_path() else {
        return Ok(false);
    };
    if !path.is_file() {
        return Ok(false);
    }
    let text = timer_unit(minutes);
    if fs::read_to_string(&path)
        .map(|old| old == text)
        .unwrap_or(false)
    {
        return Ok(false);
    }
    write_atomic(&path, &text)?;
    systemctl_checked(&["daemon-reload"])?;
    // 只重启 timer 本身，不会打断正在跑的那一轮搬运。
    // 重启会让 systemd 重算到期时间（`OnStartupSec` 已过期则立刻补跑一轮），
    // 所以改完间隔可能马上看到一轮搬运，属正常现象。
    systemctl_checked(&["restart", TIMER_NAME])?;
    Ok(true)
}

/// 立刻跑一轮：直接启动 oneshot service。
///
/// `--no-block` 是必须的：oneshot 的 `start` 默认会**等单元跑完**才返回，
/// 同步可能要几十秒，而调用方是 GUI 线程。
pub fn request_round() -> Result<bool> {
    if !installed() {
        return Ok(false);
    }
    let output = systemctl(&["start", "--no-block", SERVICE_NAME])?;
    if !output.status.success() {
        bail!(
            "请求立即搬运失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(true)
}

/// 后台活动开关变化时调用：开 → 启动 timer，关 → 停掉 timer。
///
/// 单元文件都留着；真正「是否干活」还有一层保险：`--scheduled-round` 自己
/// 会检查 `[background] enabled`，关掉时直接返回空串。
pub fn apply_background_switch(enabled: bool) -> Result<()> {
    if !installed() {
        return Ok(());
    }
    let output = if enabled {
        systemctl(&["start", TIMER_NAME])
    } else {
        systemctl(&["stop", TIMER_NAME])
    }
    .context("更新后台定时器状态失败")?;
    if !output.status.success() {
        bail!(
            "{}后台定时器失败：{}",
            if enabled { "启动" } else { "停止" },
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// 程序换过位置时修一次单元文件。
///
/// Windows 服务装时会复制 exe 到固定目录、macOS 要重新登记签名约束，
/// Linux 这边的问题是 `ExecStart` 里写的是**绝对路径**：用户把程序挪到别处后
/// 到点拉起会直接 `status=203/EXEC`，界面上只看到「timer 已开启」却永远不搬。
/// 主程序启动时比对一次，路径不一致就按当前路径重写（`--app-dir` 沿用旧值）。
pub fn heal_after_update() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Ok(service) = service_path() else {
        return;
    };
    let Ok(text) = fs::read_to_string(&service) else {
        return;
    };
    let recorded = exec_program(&text);
    let current = exe.to_string_lossy().into_owned();
    if recorded.as_deref() == Some(current.as_str()) {
        return;
    }
    let Some(app_dir) = recorded_app_dir(&text) else {
        // 解析不出来就别乱写：宁可不修，也不要生成一个猜的配置目录。
        return;
    };
    if write_atomic(&service, &service_unit(&exe, Path::new(&app_dir))).is_ok() {
        let _ = systemctl(&["daemon-reload"]);
        let _ = systemctl(&["restart", TIMER_NAME]);
    }
}

fn systemctl(arguments: &[&str]) -> Result<Output> {
    let mut command = Command::new("systemctl");
    command.arg("--user").args(arguments);
    command
        .output()
        .context("调用 systemctl 失败（系统里没有 systemd？）")
}

/// 与 [`systemctl`] 相同，但把**非零退出码**也当成失败。
///
/// `Command::output()` 只反映「有没有把 systemctl 跑起来」；unit 不存在、
/// D-Bus 不可达这类错误都体现在退出码里，光看 `?` 会把它们当成成功。
fn systemctl_checked(arguments: &[&str]) -> Result<()> {
    let output = systemctl(arguments)?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let detail = if stderr.is_empty() { stdout } else { stderr };
    bail!(
        "`systemctl --user {}` 失败：{}",
        arguments.join(" "),
        if detail.is_empty() {
            format!("退出码 {:?}", output.status.code())
        } else {
            detail
        }
    )
}

fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    // 用全局唯一的临时名：`with_extension("tmp")` 会让 `.service` 与 `.timer`
    // 共用同一个 `rime-user-dict-sync.tmp`，并发写时可能把 service 的内容
    // rename 成 timer，timer 从此永久加载失败。
    crate::hash::write_atomic(path, contents)
}

fn interval_minutes(minutes: u32) -> u32 {
    // 与 Settings 的 clamp 保持一致：1 分钟 ~ 24 小时。
    minutes.clamp(1, 24 * 60)
}

/// systemd 的 `ExecStart` 参数引用。
///
/// 双引号里 `\` 与 `"` 需要转义；`%` 是 systemd 的说明符前缀（`%h`、`%i`……），
/// 路径里真出现 `%` 时必须写成 `%%`，否则 unit 会解析失败。
fn exec_quote(value: &str) -> String {
    let escaped = value
        .replace('%', "%%")
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn service_unit(exe: &Path, app_dir: &Path) -> String {
    format!(
        "# 由 RimeUserDictSync 生成，请勿手工编辑（改设置请用界面或 --install-service）。\n\
         [Unit]\n\
         Description=RIME 用户词库同步 · 后台搬运\n\
         Documentation=https://github.com/small32/RimeUserDictSync\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         # 只搬运文件，不读不写 RIME 根目录，也不调用 /sync、/deploy，不打断输入。\n\
         ExecStart={exe} --scheduled-round --app-dir {app_dir}\n\
         # 后台轮次可以慢慢跑：让出 CPU 与 IO，别影响正在打字的人。\n\
         Nice=10\n\
         IOSchedulingClass=idle\n",
        exe = exec_quote(&exe.to_string_lossy()),
        app_dir = exec_quote(&app_dir.to_string_lossy()),
    )
}

fn timer_unit(minutes: u32) -> String {
    let minutes = interval_minutes(minutes);
    format!(
        "# 由 RimeUserDictSync 生成，请勿手工编辑（改间隔请用界面或 --install-service）。\n\
         [Unit]\n\
         Description=RIME 用户词库同步 · 定时搬运\n\
         Documentation=https://github.com/small32/RimeUserDictSync\n\
         \n\
         [Timer]\n\
         # `OnStartupSec` 是「本 user manager 启动后」——恰好在登录时；不能用\n\
         # `OnBootSec`（那是相对**机器**启动，登录时早已过去，systemd 会立刻\n\
         # 触发一次，反而和桌面启动抢资源）。\n\
         OnStartupSec=2min\n\
         # 从「上一次被激活」起算，不是「跑完」起算；单轮超过间隔时 systemd\n\
         # 会跳过重叠触发，实际周期会被拉长，这是可接受的。\n\
         OnUnitActiveSec={minutes}min\n\
         AccuracySec=30s\n\
         Unit={SERVICE_NAME}\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n"
    )
}

/// 从 timer 单元里读回间隔（只用于界面显示）。
fn interval_minutes_of_unit(text: &str) -> Option<u32> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("OnUnitActiveSec="))?;
    let value = line.trim_start_matches("OnUnitActiveSec=").trim();
    value.strip_suffix("min")?.trim().parse::<u32>().ok()
}

/// 取 `ExecStart=` 里的第一个参数（去掉引号与转义）。
fn exec_program(text: &str) -> Option<String> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("ExecStart="))?;
    let rest = line.trim_start_matches("ExecStart=").trim_start();
    unquote_first(rest)
}

/// 取 `--app-dir` 后面的那个参数。
fn recorded_app_dir(text: &str) -> Option<String> {
    let index = text.find("--app-dir ")?;
    unquote_first(&text[index + "--app-dir ".len()..])
}

fn unquote_first(value: &str) -> Option<String> {
    let value = value.trim_start();
    if let Some(rest) = value.strip_prefix('"') {
        let mut out = String::new();
        let mut escaped = false;
        let mut chars = rest.chars().peekable();
        while let Some(character) = chars.next() {
            if escaped {
                out.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                return Some(out);
            } else if character == '%' && chars.peek() == Some(&'%') {
                // `exec_quote` 把字面 `%` 写成 `%%`（systemd 的说明符前缀）；
                // 读回必须还原，否则路径里带 `%` 时这里会拿到 `100%%`，
                // 与 `current_exe()` 永不相等 → 每次启动都重写单元，
                // 且 `--app-dir` 会一路翻倍成 `%%%%`。
                chars.next();
                out.push('%');
            } else {
                out.push(character);
            }
        }
        return None;
    }
    let end = value.find(char::is_whitespace).unwrap_or(value.len());
    Some(value[..end].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_runs_the_scheduled_round_with_the_app_dir() {
        let unit = service_unit(
            Path::new("/home/me/程序/RimeUserDictSync"),
            Path::new("/home/me/Rime Config"),
        );
        assert!(unit.contains("Type=oneshot"));
        assert!(unit.contains("--scheduled-round"));
        assert!(unit.contains("--app-dir"));
        assert!(unit.contains("\"/home/me/程序/RimeUserDictSync\""));
        assert!(unit.contains("\"/home/me/Rime Config\""));
        // 后台搬运必须让出资源，否则打字会卡。
        assert!(unit.contains("Nice=10"));
    }

    #[test]
    fn timer_repeats_after_each_run_and_is_installable() {
        let unit = timer_unit(10);
        assert!(unit.contains("OnUnitActiveSec=10min"));
        // 必须是 OnStartupSec（相对 user manager 启动 = 登录时）；
        // OnBootSec 相对机器启动，登录时早已过去，会立刻触发一轮。
        assert!(unit.contains("OnStartupSec=2min"));
        assert!(
            !unit
                .lines()
                .any(|line| line.trim_start().starts_with("OnBootSec")),
            "不能留下 OnBootSec 指令（注释里提到不算）"
        );
        assert!(unit.contains("Unit=rime-user-dict-sync.service"));
        assert!(unit.contains("WantedBy=timers.target"));
    }

    #[test]
    fn interval_is_clamped_to_the_same_bounds_as_settings() {
        assert_eq!(interval_minutes(0), 1);
        assert_eq!(interval_minutes(10), 10);
        assert_eq!(interval_minutes(99_999), 24 * 60);
    }

    #[test]
    fn interval_reads_back_from_the_unit() {
        assert_eq!(interval_minutes_of_unit(&timer_unit(7)), Some(7));
        assert_eq!(interval_minutes_of_unit("[Timer]\n"), None);
    }

    #[test]
    fn percent_and_quotes_are_escaped_for_systemd() {
        let unit = service_unit(Path::new("/opt/100%/a\"b"), Path::new("/tmp/x"));
        // % 是 systemd 说明符前缀，必须写成 %%
        assert!(unit.contains("/opt/100%%/a\\\"b"));
        assert!(!unit.contains("/opt/100%/"));
    }

    #[test]
    fn exec_program_and_app_dir_are_read_back() {
        let unit = service_unit(
            Path::new("/home/me/程序 with space/RimeUserDictSync"),
            Path::new("/home/me/Rime Config"),
        );
        assert_eq!(
            exec_program(&unit).as_deref(),
            Some("/home/me/程序 with space/RimeUserDictSync")
        );
        assert_eq!(
            recorded_app_dir(&unit).as_deref(),
            Some("/home/me/Rime Config")
        );
    }

    /// 路径里带 `%` 时必须**原样读回**：否则 `heal_after_update` 会认为程序
    /// 换了位置，每次启动都重写单元，并把 `--app-dir` 一路翻倍成 `%%%%`。
    #[test]
    fn percent_paths_round_trip_without_doubling() {
        let program = Path::new("/opt/100% 完整版/RimeUserDictSync");
        let app_dir = Path::new("/home/me/词库 100%备份");
        let unit = service_unit(program, app_dir);
        assert_eq!(
            exec_program(&unit).as_deref(),
            Some("/opt/100% 完整版/RimeUserDictSync")
        );
        assert_eq!(
            recorded_app_dir(&unit).as_deref(),
            Some("/home/me/词库 100%备份")
        );

        // 用读回的值再生成一次，必须字节级等价（幂等）。
        let again = service_unit(
            Path::new(&exec_program(&unit).unwrap()),
            Path::new(&recorded_app_dir(&unit).unwrap()),
        );
        assert_eq!(again, unit);
    }
}
