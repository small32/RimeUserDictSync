//! macOS 的「Rime词库同步」：两个 LaunchAgent。
//!
//! 1. `...agent`：轻量定时搬运。`StartInterval` 到点由 launchd 拉起主程序跑
//!    `--scheduled-round`，跑完即退，没有常驻进程；
//! 2. `...menubar`：常驻菜单栏图标（`RimeMenuBar` 助手）。左键点图标 = 依次执行
//!    `Squirrel --sync` + `Squirrel --reload`，把输入法菜单里的两次点击合成一次；
//!    它不读写 WebDAV，也不做合并——那是主程序「同步词库」按钮的事。
//!
//! 图标显隐跟随 `RimeUserDictSync.ini` 的 `[background] enabled`：
//! 关掉后台活动后 5 秒内助手会移除图标并 `exit(0)`；plist 用
//! `KeepAlive = { SuccessfulExit: false }`，所以正常退出不会被 launchd 重新拉起。
//! 重新开启时由 GUI 调 `apply_background_switch(true)` 触发 `kickstart`。
//!
//! 与 Windows 服务的差异（刻意为之）：LaunchAgent 只在**用户登录后**运行，
//! 不能像 SCM 服务那样在登录前同步；但不需要管理员权限，菜单栏助手也不占 Dock。

use anyhow::{Context as _, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

pub const LABEL: &str = "com.small32.rime-user-dict-sync.agent";
pub const MENUBAR_LABEL: &str = "com.small32.rime-user-dict-sync.menubar";

/// 主程序的 bundle id。写进 plist 的 `AssociatedBundleIdentifiers`，
/// 「登录与扩展」才会用 App 的名字和图标来显示后台项目，而不是一个黑底 exec。
///
/// 两个 job 都指向它：嵌套的 `RimeMenuBar.app` 没有被 LaunchServices 注册，
/// 用 `...menubar` 这个 id 去关联会让 `launchctl bootstrap` 直接报
/// `Input/output error`（实测）。指向主程序后，两项会合并成一行「2 个项目」。
pub const MAIN_BUNDLE_ID: &str = "com.small32.rime-user-dict-sync";

fn launch_agents_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("读不到 HOME，无法定位 LaunchAgents")?;
    Ok(PathBuf::from(home).join("Library/LaunchAgents"))
}

/// 定时搬运的 plist。
pub fn plist_path() -> Result<PathBuf> {
    Ok(launch_agents_dir()?.join(format!("{LABEL}.plist")))
}

/// 菜单栏图标的 plist。
pub fn menubar_plist_path() -> Result<PathBuf> {
    Ok(launch_agents_dir()?.join(format!("{MENUBAR_LABEL}.plist")))
}

/// 任一部分已安装即视为「已开启」。
pub fn installed() -> bool {
    plist_path().map(|path| path.is_file()).unwrap_or(false)
        || menubar_plist_path()
            .map(|path| path.is_file())
            .unwrap_or(false)
}

fn menubar_installed() -> bool {
    menubar_plist_path()
        .map(|path| path.is_file())
        .unwrap_or(false)
}

/// 定时搬运任务是否已被 launchd 加载（GUI 内建线程据此让位）。
pub fn loaded() -> bool {
    job_loaded(LABEL)
}

/// UI 已写入 wake 后立即触发定时任务；不终止已经在执行的同步进程。
pub fn request_round() -> Result<bool> {
    if !loaded() {
        return Ok(false);
    }
    let service = format!("gui/{}/{LABEL}", uid()?);
    let output = launchctl(&["kickstart", &service])?;
    if !output.status.success() {
        bail!(
            "请求立即搬运失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(true)
}

/// 任务当前是否有进程在跑（菜单栏助手用）。
fn job_running(label: &str) -> bool {
    let Ok(uid) = uid() else {
        return false;
    };
    let Ok(output) = launchctl(&["print", &format!("gui/{uid}/{label}")]) else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("state = running") || line.starts_with("pid =")
    })
}

/// 状态描述，交给 UI 直接显示。
pub fn describe() -> String {
    status().0
}

/// 一次查询同时拿到「描述文本」与「定时搬运是否加载」。
pub fn status() -> (String, bool) {
    let periodic_path = plist_path().ok();
    let menu_path = menubar_plist_path().ok();
    let periodic_installed = periodic_path.as_ref().is_some_and(|path| path.is_file());
    let menu_installed = menu_path.as_ref().is_some_and(|path| path.is_file());

    if !periodic_installed && !menu_installed {
        return ("Rime词库同步：未开启".to_owned(), false);
    }

    let minutes = periodic_path
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| start_interval_seconds(&text))
        .map(|seconds| seconds / 60)
        .unwrap_or(0);

    let mut text = String::from("Rime词库同步：已开启");
    if periodic_installed {
        text.push_str(&format!("（每 {minutes} 分钟搬运一轮）"));
    }
    if menu_installed {
        if job_running(MENUBAR_LABEL) {
            text.push_str("；菜单栏图标已启动");
        } else {
            text.push_str("；菜单栏图标未启动");
        }
    }
    (text, loaded())
}

/// 安装/更新两个 LaunchAgent。
///
/// 定时搬运沿用「后台搬运间隔」；菜单栏助手没有间隔，它只在点击时动作。
pub fn install(app_dir: &Path, minutes: u32) -> Result<String> {
    let exe = std::env::current_exe().context("无法确定程序路径")?;
    if !exe.is_file() {
        bail!("程序路径不存在：{}", exe.display());
    }
    let helper = menu_bar_helper(&exe)?;

    let periodic = plist_path()?;
    let menu = menubar_plist_path()?;
    if let Some(parent) = periodic.parent() {
        fs::create_dir_all(parent).context("创建 LaunchAgents 目录失败")?;
    }

    write_atomic(&periodic, &plist_xml(&exe, app_dir, minutes))?;
    bootstrap(LABEL, &periodic)?;

    write_atomic(&menu, &menubar_plist_xml(&helper, app_dir))?;
    bootstrap(MENUBAR_LABEL, &menu)?;

    Ok(format!(
        "已开启 Rime词库同步：\n\
         · 后台搬运：每 {minutes} 分钟一轮\n\
         · 菜单栏图标：左键点一下 = 同步并部署；右键有菜单\n\
         · plist：{}、{}",
        periodic.display(),
        menu.display()
    ))
}

/// 关闭并移除两个 LaunchAgent。
pub fn uninstall() -> Result<String> {
    let periodic = plist_path()?;
    let menu = menubar_plist_path()?;
    if !periodic.is_file() && !menu.is_file() {
        return Ok("没有开启 Rime词库同步，无需关闭。".to_owned());
    }
    let uid = uid()?;
    for (label, path) in [(LABEL, &periodic), (MENUBAR_LABEL, &menu)] {
        if path.is_file() {
            let _ = launchctl(&["bootout", &format!("gui/{uid}/{label}")]);
            fs::remove_file(path).with_context(|| format!("删除 {} 失败", path.display()))?;
        }
    }
    Ok("已关闭 Rime词库同步：后台搬运与菜单栏图标均已移除。".to_owned())
}

/// 主程序启动（或重新打开界面）时把菜单栏助手拉起来。
///
/// 为什么需要它：「隐藏图标」只让助手正常退出，plist 还在，但 launchd
/// **不会**再自动拉它 —— `KeepAlive = { SuccessfulExit: false }` 的语义是
/// 「只在非正常退出时重启」，正常退出就让它安静地躺着。所以重新运行 App
/// 时必须显式 `kickstart`，否则用户会发现图标没了，而且无从找回
/// （除了注销重登或者重新点一次后台开关）。
pub fn ensure_menubar_running() {
    // 没装、被用户在「登录与扩展」里关掉、或者已经在跑 —— 都不用管。
    if !menubar_installed() || is_disabled(MENUBAR_LABEL) || job_running(MENUBAR_LABEL) {
        return;
    }
    let Ok(uid) = uid() else {
        return;
    };
    let service = format!("gui/{uid}/{MENUBAR_LABEL}");
    // 不带 -k：job 若在跑就原样保留，避免把图标闪一下重启。
    let _ = launchctl(&["kickstart", &service]);
}

/// 改间隔：只影响定时搬运的 plist，内容没变就不重新加载。
pub fn refresh_interval(app_dir: &Path, minutes: u32) -> Result<bool> {
    if !installed() {
        return Ok(false);
    }
    let exe = std::env::current_exe().context("无法确定程序路径")?;
    let path = plist_path()?;
    let xml = plist_xml(&exe, app_dir, minutes);
    if fs::read_to_string(&path)
        .map(|old| old == xml)
        .unwrap_or(false)
    {
        return Ok(false);
    }
    write_atomic(&path, &xml)?;
    bootstrap(LABEL, &path)?;
    Ok(true)
}

/// 后台活动开关变化时调用：
/// 开 → 把助手 `kickstart` 起来（它可能因为上次关闭而正常退出了）；
/// 关 → `SIGTERM` 掉助手，图标立即消失。
pub fn apply_background_switch(enabled: bool) -> Result<()> {
    if !menubar_installed() {
        return Ok(());
    }
    let uid = uid()?;
    let service = format!("gui/{uid}/{MENUBAR_LABEL}");
    let output = if enabled {
        launchctl(&["kickstart", "-k", &service])
    } else {
        launchctl(&["kill", "SIGTERM", &service])
    }
    .context("更新菜单栏助手状态失败")?;
    if !output.status.success() {
        bail!(
            "{}菜单栏助手失败：{}",
            if enabled { "启动" } else { "停止" },
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// 程序升级后修一次后台任务。
///
/// launchd 在 bootstrap 时会把当时那个可执行文件的**代码签名**记成一条
/// 「启动约束」（`Launch Constraint`，`launchctl print` 里能看到 `has LWCR`）。
/// 之后只要二进制被替换过、签名哈希变了，到点拉起就会直接被系统杀掉，
/// 崩溃报告写的是 `SIGKILL (Code Signature Invalid) / Launch Constraint Violation`。
/// 在「登录与扩展」里把条目关掉再打开，恰好会触发这次拉起，所以那里最容易撞见。
///
/// 修法：主程序启动时比较 plist 与可执行文件的修改时间，发现二进制更新过就
/// 重新 bootstrap 一次（等价于重新登记约束）。用户主动关掉的条目不动。
pub fn heal_after_update() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let jobs = [
        (LABEL, plist_path().ok(), Some(exe.clone())),
        (
            MENUBAR_LABEL,
            menubar_plist_path().ok(),
            menu_bar_helper(&exe).ok(),
        ),
    ];
    for (label, plist, program) in jobs {
        let (Some(plist), Some(program)) = (plist, program) else {
            continue;
        };
        if !plist.is_file() || is_disabled(label) {
            continue;
        }
        // 两种需要修的情况：
        //  1) 二进制比 plist 新 —— 签名约束过期，到点拉起会被系统杀掉；
        //  2) 任务压根不在 launchd 里 —— 上一次重载失败时 bootout 已经把条目删了，
        //     不补回去的话图标/搬运会永久消失（用户只看到"功能没了"）。
        let stale = binary_is_newer(&program, &plist);
        let missing = !job_loaded(label);
        if !stale && !missing {
            continue;
        }
        // 只有真的加载成功才推进 mtime，否则下次启动还要再试。
        if bootstrap_with_retry(label, &plist) {
            let _ = filetime::set_file_mtime(&plist, filetime::FileTime::now());
        }
    }
}

fn job_loaded(label: &str) -> bool {
    let Ok(uid) = uid() else {
        return false;
    };
    launchctl(&["print", &format!("gui/{uid}/{label}")])
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// `bootstrap` 内部会先 `bootout` 再加载，失败时任务就"消失"了，所以必须重试。
fn bootstrap_with_retry(label: &str, plist: &Path) -> bool {
    for attempt in 0..3 {
        if bootstrap(label, plist).is_ok() {
            return true;
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(800));
        }
    }
    false
}

/// 可执行文件比 plist 新 = 程序换过版本，launchd 里的签名约束已过期。
fn binary_is_newer(program: &Path, plist: &Path) -> bool {
    let modified = |path: &Path| fs::metadata(path).and_then(|meta| meta.modified());
    match (modified(program), modified(plist)) {
        (Ok(program), Ok(plist)) => program > plist,
        _ => false,
    }
}

/// 条目是否被用户在「登录与扩展」里关掉了；关掉就别强行拉起来。
fn is_disabled(label: &str) -> bool {
    let Ok(uid) = uid() else {
        return false;
    };
    let Ok(output) = launchctl(&["print-disabled", &format!("gui/{uid}")]) else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        let line = line.trim();
        line.starts_with(&format!("\"{label}\"")) && line.ends_with("=> disabled")
    })
}

fn menu_bar_helper(exe: &Path) -> Result<PathBuf> {
    let helper = exe
        .parent()
        .context("无法定位程序目录")?
        .parent()
        .context("无法定位 Contents 目录")?
        .join("Resources/RimeMenuBar.app/Contents/MacOS/RimeMenuBar");
    if !helper.is_file() {
        bail!(
            "当前构建缺少菜单栏助手 {}（需要用 build-macos.sh 打包完整 .app）",
            helper.display()
        );
    }
    Ok(helper)
}

fn bootstrap(label: &str, path: &Path) -> Result<()> {
    let uid = uid()?;
    let domain = format!("gui/{uid}");
    let service = format!("{domain}/{label}");
    // 旧任务可能还在，bootout 失败（本来就没加载）是正常的。
    let _ = launchctl(&["bootout", &service]);
    let output = launchctl(&["bootstrap", &domain, &path.to_string_lossy()])?;
    if !output.status.success() {
        bail!(
            "launchctl bootstrap {label} 失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let _ = launchctl(&["enable", &service]);
    Ok(())
}

fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let temp = path.with_extension("plist.tmp");
    fs::write(&temp, contents).with_context(|| format!("写入 {} 失败", temp.display()))?;
    fs::rename(&temp, path).with_context(|| format!("替换 {} 失败", path.display()))?;
    Ok(())
}

fn launchctl(args: &[&str]) -> Result<Output> {
    Command::new("/bin/launchctl")
        .args(args)
        .output()
        .context("调用 launchctl 失败")
}

fn uid() -> Result<String> {
    let output = Command::new("/usr/bin/id").arg("-u").output()?;
    if !output.status.success() {
        bail!("无法获取当前用户 UID");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn interval_seconds(minutes: u32) -> u64 {
    // 与 Settings 的 clamp 保持一致：1 分钟 ~ 24 小时。
    u64::from(minutes.clamp(1, 24 * 60)) * 60
}

fn plist_xml(exe: &Path, app_dir: &Path, minutes: u32) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>--scheduled-round</string>
        <string>--app-dir</string>
        <string>{app_dir}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>StartInterval</key>
    <integer>{seconds}</integer>
    <key>AssociatedBundleIdentifiers</key>
    <array>
        <string>{bundle_id}</string>
    </array>
    <key>ProcessType</key>
    <string>Background</string>
    <key>LowPriorityIO</key>
    <true/>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
</dict>
</plist>
"#,
        label = LABEL,
        exe = xml_escape(&exe.to_string_lossy()),
        app_dir = xml_escape(&app_dir.to_string_lossy()),
        seconds = interval_seconds(minutes),
        bundle_id = MAIN_BUNDLE_ID,
    )
}

fn menubar_plist_xml(helper: &Path, app_dir: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{helper}</string>
        <string>--app-dir</string>
        <string>{app_dir}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>AssociatedBundleIdentifiers</key>
    <array>
        <string>{bundle_id}</string>
    </array>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
</dict>
</plist>
"#,
        label = MENUBAR_LABEL,
        helper = xml_escape(&helper.to_string_lossy()),
        app_dir = xml_escape(&app_dir.to_string_lossy()),
        bundle_id = MAIN_BUNDLE_ID,
    )
}

/// 从 plist 里读 `StartInterval`，只用于 UI 显示。
fn start_interval_seconds(text: &str) -> Option<u64> {
    let after_key = text.split("<key>StartInterval</key>").nth(1)?;
    let after_open = after_key.split("<integer>").nth(1)?;
    after_open.split("</integer>").next()?.trim().parse().ok()
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periodic_plist_uses_the_scheduled_round_command() {
        let xml = plist_xml(
            Path::new("/Applications/RIME 工具/RimeUserDictSync"),
            Path::new("/Users/me/Rime Config"),
            10,
        );
        assert!(xml.contains("<string>--scheduled-round</string>"));
        assert!(xml.contains("<string>--app-dir</string>"));
        assert!(xml.contains("<string>/Users/me/Rime Config</string>"));
        assert!(xml.contains("<integer>600</integer>"));
        assert!(xml.contains("<string>Aqua</string>"));
        // 「登录与扩展」靠这个键把项目归到 App 名下（名字与图标都取自该 bundle）。
        assert!(xml.contains("<key>AssociatedBundleIdentifiers</key>"));
        assert!(xml.contains(&format!("<string>{MAIN_BUNDLE_ID}</string>")));
    }

    #[test]
    fn menubar_plist_runs_the_helper_and_stays_alive() {
        let xml = menubar_plist_xml(
            Path::new("/Applications/RimeUserDictSync.app/Contents/MacOS/RimeMenuBar"),
            Path::new("/Users/me/Rime Config"),
        );
        assert!(xml.contains("RimeMenuBar"));
        assert!(xml.contains("<string>--app-dir</string>"));
        assert!(xml.contains("<key>KeepAlive</key>"));
        assert!(xml.contains("<key>SuccessfulExit</key>"));
        assert!(xml.contains("<false/>"));
        assert!(xml.contains("<string>Interactive</string>"));
        // 登录项里显示主程序的名字与图标（不能用嵌套助手的 id，LaunchServices 不认）。
        assert!(xml.contains(&format!("<string>{MAIN_BUNDLE_ID}</string>")));
    }

    #[test]
    fn binary_newer_than_plist_marks_a_stale_launch_constraint() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("job.plist");
        let program = dir.path().join("program");
        std::fs::write(&program, b"x").unwrap();
        std::fs::write(&plist, b"y").unwrap();

        let older = filetime::FileTime::from_unix_time(1_000_000, 0);
        let newer = filetime::FileTime::from_unix_time(2_000_000, 0);
        filetime::set_file_mtime(&plist, newer).unwrap();
        filetime::set_file_mtime(&program, older).unwrap();
        assert!(!binary_is_newer(&program, &plist));

        filetime::set_file_mtime(&program, newer).unwrap();
        filetime::set_file_mtime(&plist, older).unwrap();
        assert!(binary_is_newer(&program, &plist));

        assert!(!binary_is_newer(&dir.path().join("missing"), &plist));
    }

    #[test]
    fn interval_is_clamped_to_the_same_bounds_as_settings() {
        assert_eq!(interval_seconds(0), 60);
        assert_eq!(interval_seconds(10), 600);
        assert_eq!(interval_seconds(99_999), 86_400);
    }

    #[test]
    fn start_interval_is_read_back() {
        let xml = plist_xml(Path::new("/x"), Path::new("/y"), 7);
        assert_eq!(start_interval_seconds(&xml), Some(420));
    }

    #[test]
    fn paths_are_xml_escaped() {
        let xml = plist_xml(Path::new("/tmp/a&b<c>.app"), Path::new("/tmp/dir"), 5);
        assert!(xml.contains("/tmp/a&amp;b&lt;c&gt;.app"));
        assert!(!xml.contains("a&b"));
    }
}
