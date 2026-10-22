//! Linux 快捷方式「同步词库」：freedesktop `.desktop` 菜单项。
//!
//! 对应关系：Windows 生成 `.lnk`（`shortcut.rs`），macOS 生成 AppleScript `.app`
//! （`shortcut_macos.rs`），Linux 生成 `~/.local/share/applications/` 下的
//! `.desktop`——这是 Linux 上「开始菜单里的一个图标」的标准做法，
//! 不需要用户自己去拖文件。
//!
//! 与 macOS 一致，双击执行的是**完整一轮** `--sync-now`：
//! 后台搬运只搬文件，不做并集合并，`Sync/Others/` 若不在这里写回
//! `RIME/` 就永远到不了输入法。Windows 那边只做 `/sync` + `/deploy`
//! 是因为它的后台服务本来就负责搬运，两端取舍不同（见 README「快捷方式」）。
//!
//! 图标不用通用脚本图标：`assets/sync-gear.ico`（就是 Windows 快捷方式那个齿轮）
//! 编译期嵌进二进制，安装时解成 PNG 写到 XDG 图标目录，`Icon=` 直接用绝对路径
//! ——省掉 `gtk-update-icon-cache` 与主题查找这两层不确定性。
//!
//! 解图不能直接交给 `image::load_from_memory`：那个 ICO 里的帧是 **PNG 压缩的
//! RGB**，而 `image` 的 ICO 解码器只接受 RGBA 的嵌入 PNG，会直接
//! `PngNotRgba` 报错。所以这里自己走一遍 ICO 目录，取最大的一帧当 PNG 解。
//! 另外图标是白底的，白底在深色面板上就是一个白方块，顺手把近白像素抠成透明。

use anyhow::{Context as _, Result, bail};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub const SHORTCUT_TITLE: &str = "同步词库";

/// 菜单项文件名。带前缀是为了在 `applications/` 里一眼看出是谁生成的。
const DESKTOP_NAME: &str = "rime-user-dict-sync.desktop";
const ICON_NAME: &str = "rime-user-dict-sync.png";

/// 标记字段：证明这个 `.desktop` 是本工具生成的。
///
/// 删除/覆盖前必须校验它，避免把用户自己写的同名菜单项删掉。
const MARKER: &str = "X-RimeUserDictSync=1";

const ICON_BYTES: &[u8] = include_bytes!("../assets/sync-gear.ico");

fn applications_dir() -> Result<PathBuf> {
    Ok(data_home()?.join("applications"))
}

fn icons_dir() -> Result<PathBuf> {
    Ok(data_home()?.join("icons"))
}

fn data_home() -> Result<PathBuf> {
    match std::env::var_os("XDG_DATA_HOME") {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => {
            let home = std::env::var_os("HOME").context("读不到 HOME，无法定位应用菜单目录")?;
            Ok(PathBuf::from(home).join(".local/share"))
        }
    }
}

/// 快捷方式位置（菜单项文件）。
pub fn path() -> Result<PathBuf> {
    Ok(applications_dir()?.join(DESKTOP_NAME))
}

pub fn exists() -> bool {
    path().map(|path| path.is_file()).unwrap_or(false)
}

pub fn describe() -> String {
    match path() {
        Ok(target) if target.is_file() => {
            format!("快捷方式「{SHORTCUT_TITLE}」：{}", target.display())
        }
        Ok(target) => format!(
            "快捷方式「{SHORTCUT_TITLE}」：未创建（会建到 {}）",
            target.display()
        ),
        Err(error) => format!("快捷方式「{SHORTCUT_TITLE}」：{error}"),
    }
}

pub fn install(app_dir: &Path) -> Result<String> {
    let exe = std::env::current_exe().context("无法确定程序路径")?;
    if !exe.is_file() {
        bail!("程序路径不存在：{}", exe.display());
    }
    let target = path()?;
    if target.is_file() && !is_ours(&target) {
        bail!(
            "{} 已存在且不是本工具创建的菜单项，请先手动移走再重试",
            target.display()
        );
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).context("创建应用菜单目录失败")?;
    }

    // 图标失败不算致命：退回主题里的通用图标，功能不受影响。
    let icon = write_icon().unwrap_or_else(|| PathBuf::from("accessories-dictionary"));
    fs::write(&target, desktop_entry(&exe, app_dir, &icon))
        .with_context(|| format!("写入 {} 失败", target.display()))?;
    // 有些文件管理器要求菜单项可执行才允许双击运行。
    let _ = fs::set_permissions(&target, fs::Permissions::from_mode(0o755));
    refresh_menu_database();

    Ok(format!(
        "已创建快捷方式「{SHORTCUT_TITLE}」：{}\n\
         在开始菜单里搜「{SHORTCUT_TITLE}」即可运行（也可把它拖到桌面或面板）。\n\
         它执行的是完整一轮：拉取 → 并集合并 → 写回本地 → 上传 → RIME 同步 → 重新部署。\n\
         主程序位置变化后请重新创建。",
        target.display()
    ))
}

pub fn uninstall() -> Result<String> {
    let target = path()?;
    if !target.is_file() {
        return Ok(format!("没有找到「{SHORTCUT_TITLE}」快捷方式，无需删除。"));
    }
    if !is_ours(&target) {
        bail!(
            "{} 不是本工具创建的，拒绝删除；请确认后手动处理",
            target.display()
        );
    }
    fs::remove_file(&target).with_context(|| format!("删除 {} 失败", target.display()))?;
    if let Ok(icon) = icons_dir().map(|dir| dir.join(ICON_NAME)) {
        let _ = fs::remove_file(icon);
    }
    refresh_menu_database();
    Ok(format!("已删除快捷方式「{SHORTCUT_TITLE}」。"))
}

fn is_ours(entry: &Path) -> bool {
    fs::read_to_string(entry)
        .map(|text| text.lines().any(|line| line.trim() == MARKER))
        .unwrap_or(false)
}

/// 把内嵌的 `.ico` 解成 PNG。失败时返回 `None`，由调用方退回通用图标。
fn write_icon() -> Option<PathBuf> {
    let image = icon_rgba()?;
    let dir = icons_dir().ok()?;
    fs::create_dir_all(&dir).ok()?;
    let path = dir.join(ICON_NAME);
    image.save(&path).ok()?;
    Some(path)
}

/// 解出最大的一帧，去掉白底。
fn icon_rgba() -> Option<image::RgbaImage> {
    let frame = largest_ico_frame(ICON_BYTES)?;
    let image = image::load_from_memory(frame).ok()?.into_rgba8();
    Some(clear_white_background(image))
}

/// 按 ICO 目录挑出面积最大的一帧，返回它的字节切片。
///
/// 布局：6 字节文件头（`type` 在偏移 2，帧数在偏移 4），随后每帧 16 字节目录项，
/// `bytesInRes` 在项内偏移 8，`imageOffset` 在偏移 12（都是小端 u32）。
fn largest_ico_frame(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() < 6 || bytes[2] != 1 || bytes[3] != 0 {
        return None;
    }
    let count = usize::from(u16::from_le_bytes([bytes[4], bytes[5]]));
    let mut best: Option<(u32, &[u8])> = None;
    for index in 0..count {
        let start = 6 + index * 16;
        let entry = bytes.get(start..start + 16)?;
        // 0 表示 256（老格式里一个字节放不下 256）。
        let width = if entry[0] == 0 {
            256
        } else {
            u32::from(entry[0])
        };
        let height = if entry[1] == 0 {
            256
        } else {
            u32::from(entry[1])
        };
        let length = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize;
        let offset = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize;
        let frame = bytes.get(offset..offset.checked_add(length)?)?;
        let area = width * height;
        if best.is_none_or(|(best_area, _)| area > best_area) {
            best = Some((area, frame));
        }
    }
    best.map(|(_, frame)| frame)
}

/// 近白像素抠成透明：图标原图是白底的。只动白到发亮的部分，
/// 齿轮本身的灰阶不受影响。
fn clear_white_background(mut image: image::RgbaImage) -> image::RgbaImage {
    for pixel in image.pixels_mut() {
        let [red, green, blue, alpha] = pixel.0;
        if alpha > 0 && red >= 245 && green >= 245 && blue >= 245 {
            *pixel = image::Rgba([red, green, blue, 0]);
        }
    }
    image
}

/// 尽力刷新菜单缓存；`update-desktop-database` 不存在也不影响使用。
fn refresh_menu_database() {
    let Ok(dir) = applications_dir() else {
        return;
    };
    let _ = Command::new("update-desktop-database")
        .arg(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// `.desktop` 的 `Exec` 参数引用（freedesktop Desktop Entry 规范）。
///
/// - 含空白或保留字符时整体加双引号，引号内 `"` `` ` `` `$` `\` 反斜杠转义；
/// - `%` 是字段码前缀（`%f`、`%u`……），字面量必须写成 `%%`。
fn exec_quote(value: &str) -> String {
    let value = value.replace('%', "%%");
    let reserved = "\"'\\><~|&;$*?#()`";
    let needs_quote = value.is_empty()
        || value
            .chars()
            .any(|c| c.is_whitespace() || reserved.contains(c));
    if !needs_quote {
        return value;
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        if matches!(character, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(character);
    }
    out.push('"');
    out
}

fn desktop_entry(exe: &Path, app_dir: &Path, icon: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.0\n\
         Name={SHORTCUT_TITLE}\n\
         Name[en]=RIME Sync\n\
         Comment=拉取 → 合并写回 → 上传 → RIME 同步 → 重新部署\n\
         Exec={exe} --sync-now --notify --app-dir {app_dir}\n\
         Icon={icon}\n\
         Terminal=false\n\
         StartupNotify=false\n\
         Categories=Utility;\n\
         {MARKER}\n",
        exe = exec_quote(&exe.to_string_lossy()),
        app_dir = exec_quote(&app_dir.to_string_lossy()),
        icon = icon.to_string_lossy(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_runs_a_full_sync_and_carries_the_marker() {
        let entry = desktop_entry(
            Path::new("/home/me/程序/RimeUserDictSync"),
            Path::new("/home/me/Rime Config"),
            Path::new("/home/me/.local/share/icons/rime-user-dict-sync.png"),
        );
        assert!(entry.starts_with("[Desktop Entry]"));
        assert!(entry.contains("Name=同步词库"));
        // macOS 的 applet 也是这样：后台只搬运，合并写回必须在这一步做。
        assert!(entry.contains("--sync-now"));
        assert!(entry.contains("--notify"));
        assert!(entry.contains("Terminal=false"));
        assert!(entry.contains(MARKER));
        assert!(entry.contains("\"/home/me/Rime Config\""));
    }

    #[test]
    fn exec_quotes_paths_and_escapes_reserved_characters() {
        assert_eq!(exec_quote("/usr/bin/x"), "/usr/bin/x");
        assert_eq!(exec_quote("/opt/RIME 工具/x"), "\"/opt/RIME 工具/x\"");
        // % 是字段码前缀，字面量要写成 %%
        assert_eq!(exec_quote("/opt/100%/x"), "/opt/100%%/x");
        assert_eq!(exec_quote("/tmp/a$b"), "\"/tmp/a\\$b\"");
        assert_eq!(exec_quote("/tmp/a\"b"), "\"/tmp/a\\\"b\"");
    }

    #[test]
    fn marker_check_only_accepts_our_own_entry() {
        let dir = tempfile::tempdir().unwrap();
        let ours = dir.path().join("ours.desktop");
        let other = dir.path().join("other.desktop");
        fs::write(
            &ours,
            desktop_entry(Path::new("/x"), Path::new("/y"), Path::new("/z")),
        )
        .unwrap();
        fs::write(&other, "[Desktop Entry]\nName=别的\nExec=foo\n").unwrap();
        assert!(is_ours(&ours));
        assert!(!is_ours(&other));
    }

    #[test]
    fn largest_ico_frame_is_picked_and_decodes() {
        // 图标取自 assets/sync-gear.ico；直接 load_from_memory 会 PngNotRgba，
        // 所以必须走「目录 → 最大帧 → PNG」这条路。
        assert!(image::load_from_memory(ICON_BYTES).is_err());
        let frame = largest_ico_frame(ICON_BYTES).expect("ICO 目录应能解析");
        let image = image::load_from_memory(frame).unwrap().into_rgba8();
        assert!(image.width() >= 16);
        assert_eq!(image.width(), image.height());
    }

    #[test]
    fn white_background_becomes_transparent_but_the_gear_stays() {
        let image = clear_white_background(icon_rgba().expect("图标应能解码"));
        let alphas: Vec<u8> = image.pixels().map(|pixel| pixel.0[3]).collect();
        // 白底被抠掉，齿轮本体仍然不透明——两者都得有，否则等于把图标抠没了。
        assert!(alphas.contains(&0));
        assert!(alphas.contains(&255));
    }
}
