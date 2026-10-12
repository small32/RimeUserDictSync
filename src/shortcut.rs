//! 快捷方式「同步词库」。
//!
//! 后台服务只搬运文件，`/sync` 与 `/deploy` 必须在用户登录会话里执行，
//! 所以需要一个入口让用户"点一下就让远端内容生效"。
//!
//! 快捷方式就创建在**程序所在目录**（不往桌面塞东西），需要的话自己拖到
//! 桌面、开始菜单或任务栏。
//! 它指向 `RimeUserDictSync.exe --sync-now --notify --app-dir <配置目录>`。

use anyhow::{Context as _, Result};
use std::{
    fs,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub const SHORTCUT_NAME: &str = "同步词库.lnk";
pub const SHORTCUT_TITLE: &str = "同步词库";

/// 齿轮图标在 EXE 里的资源 ID（`build.rs` 用 winres 嵌入 `assets/sync-gear.ico`）。
/// EXE 自己的图标是 ID 1（weasel），两者互不影响。
pub const ICON_RESOURCE_ID: i32 = 101;

/// 写进 `.lnk` 头部的图标索引。负数表示"按资源 ID 取图标"。
pub const ICON_INDEX: i32 = -ICON_RESOURCE_ID;

/// `ShellLinkHeader.IconIndex` 在 `.lnk` 里的字节偏移。
///
/// 用十六进制核对过 Windows 自己写出来的 .lnk：`IconLocation` 字段里**只有路径**，
/// 索引存在这个头部字段里（`0` / `3` / `-101` 分别对应三个测试文件）。
/// mslnk 没有暴露这个字段，也**不会**替我们写它，所以生成后自己补上。
const ICON_INDEX_OFFSET: u64 = 56;

/// 上一版把图标散成了一个单独的 `sync-gear.ico`；升级后清理掉它。
pub const LEGACY_ICON_FILE: &str = "sync-gear.ico";

/// `IconLocation` 字段的值：**只能是裸路径**。
///
/// 把 `路径,-101` 塞进这个字符串，资源管理器会把整个东西当成文件名去找，
/// 报「Windows 找不到文件 xxxxxx.exe,-101」，图标也是空的 —— 索引归头部字段管。
fn icon_location(exe: &Path) -> String {
    exe.to_string_lossy().into_owned()
}

/// 把图标索引补写进 `.lnk` 头部。mslnk 不支持这个字段，只能生成后改这 4 个字节。
fn write_icon_index(lnk: &Path, index: i32) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(lnk)
        .with_context(|| format!("打开 {} 写图标索引失败", lnk.display()))?;
    file.seek(SeekFrom::Start(ICON_INDEX_OFFSET))?;
    file.write_all(&index.to_le_bytes())?;
    file.sync_all()?;
    Ok(())
}

/// 快捷方式位置：与 `RimeUserDictSync.exe` 同目录。
pub fn path(app_dir: &Path) -> PathBuf {
    app_dir.join(SHORTCUT_NAME)
}

pub fn exists(app_dir: &Path) -> bool {
    path(app_dir).is_file()
}

/// 状态描述，交给 UI 直接显示。
pub fn describe(app_dir: &Path) -> String {
    let target = path(app_dir);
    if target.is_file() {
        format!("快捷方式「{SHORTCUT_TITLE}」：{}", target.display())
    } else {
        format!(
            "快捷方式「{SHORTCUT_TITLE}」：未创建（会建在 {}）",
            app_dir.display()
        )
    }
}

pub fn install(app_dir: &Path) -> Result<String> {
    let exe = std::env::current_exe().context("无法确定程序路径")?;
    if !exe.is_file() {
        anyhow::bail!("程序路径不存在：{}", exe.display());
    }
    let target = path(app_dir);
    // 上一版会散出一个 sync-gear.ico，现在图标从 EXE 资源里取，顺手清掉它。
    let _ = fs::remove_file(app_dir.join(LEGACY_ICON_FILE));
    let mut link = mslnk::ShellLink::new(&exe).context("构造快捷方式失败")?;
    link.set_name(Some(SHORTCUT_TITLE.to_owned()));
    // 快捷方式自己处理搬运与合并，最后才调用 RIME 的 /sync 与 /deploy。
    link.set_arguments(Some(format!(
        "--sync-now --notify --app-dir {}",
        crate::platform::quote_windows_argument(&app_dir.to_string_lossy())
    )));
    if let Some(parent) = exe.parent() {
        link.set_working_dir(Some(parent.to_string_lossy().into_owned()));
    }
    // IconLocation 只放路径，索引另外补写进头部（见 write_icon_index）。
    link.set_icon_location(Some(icon_location(&exe)));
    if target.exists() {
        fs::remove_file(&target).with_context(|| format!("覆盖 {} 失败", target.display()))?;
    }
    link.create_lnk(&target).with_context(|| {
        format!(
            "创建 {} 失败（请确认对该目录有写权限，或换一个可写目录）",
            target.display()
        )
    })?;
    write_icon_index(&target, ICON_INDEX)?;
    Ok(format!(
        "已创建快捷方式：{}\n图标取自 {} 内嵌的齿轮资源（资源 ID {}），EXE 自己的图标不受影响，目录里也不需要额外的 .ico 文件。\n双击它会执行：拉取远端 → 并集合并 → 写回本地 → 上传 → RIME /sync → /deploy。",
        target.display(),
        exe.display(),
        ICON_RESOURCE_ID
    ))
}

pub fn uninstall(app_dir: &Path) -> Result<String> {
    let target = path(app_dir);
    let _ = fs::remove_file(app_dir.join(LEGACY_ICON_FILE));
    if !target.is_file() {
        return Ok(format!("没有找到「{SHORTCUT_TITLE}」快捷方式，无需删除。"));
    }
    fs::remove_file(&target).with_context(|| format!("删除 {} 失败", target.display()))?;
    Ok(format!("已删除快捷方式「{SHORTCUT_TITLE}」。"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_lives_next_to_the_executable() {
        let dir = Path::new(r"D:\Tools\RIME\Rime_Sync");
        assert_eq!(path(dir), dir.join(SHORTCUT_NAME));
        assert!(SHORTCUT_NAME.ends_with(".lnk"));
        assert!(SHORTCUT_NAME.starts_with(SHORTCUT_TITLE));
    }

    /// `.lnk` 的形状必须和 Windows 自己写出来的一致：
    /// `ICON_LOCATION` 是裸路径，图标索引在头部的 `IconIndex` 字段里。
    /// 这正是之前一直不生效的原因 —— 索引曾被塞进路径字符串。
    #[test]
    fn shortcut_puts_the_icon_path_and_index_in_the_right_places() {
        let dir = tempfile::tempdir().unwrap();
        // 测试进程自己就是一个可执行文件，拿它当目标。
        let exe = std::env::current_exe().unwrap();
        let lnk = dir.path().join("t.lnk");
        let mut link = mslnk::ShellLink::new(&exe).unwrap();
        // 按 install() 的真实写法填，顺带覆盖字段顺序
        link.set_name(Some(SHORTCUT_TITLE.to_owned()));
        link.set_arguments(Some("--sync-now --notify".to_owned()));
        link.set_icon_location(Some(icon_location(&exe)));
        link.create_lnk(&lnk).unwrap();
        write_icon_index(&lnk, ICON_INDEX).unwrap();

        let bytes = fs::read(&lnk).unwrap();
        assert_eq!(
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            0x4c,
            "ShellLinkHeader 大小不对"
        );
        let flags = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        assert!(flags & 0x40 != 0, "HasIconLocation 位没置上");

        // StringData 里只该有路径，没有 ",索引" 尾巴
        let stored = stored_icon_location(&bytes, flags);
        assert_eq!(stored, exe.to_string_lossy(), "IconLocation 不是裸路径");
        assert!(!stored.contains(','), "IconLocation 里混进了逗号/索引");

        // 索引在头部偏移 56，负数表示按资源 ID 取
        let index = i32::from_le_bytes([
            bytes[ICON_INDEX_OFFSET as usize],
            bytes[ICON_INDEX_OFFSET as usize + 1],
            bytes[ICON_INDEX_OFFSET as usize + 2],
            bytes[ICON_INDEX_OFFSET as usize + 3],
        ]);
        assert_eq!(index, -101, "头部 IconIndex 没写对");
    }

    /// 按 MS-SHLLINK 的结构把 ICON_LOCATION 取出来，用于断言真实字节。
    fn stored_icon_location(bytes: &[u8], flags: u32) -> String {
        let mut position = 0x4c;
        if flags & 0x1 != 0 {
            let size = u16::from_le_bytes([bytes[position], bytes[position + 1]]) as usize;
            position += 2 + size;
        }
        if flags & 0x2 != 0 {
            let size = u32::from_le_bytes([
                bytes[position],
                bytes[position + 1],
                bytes[position + 2],
                bytes[position + 3],
            ]) as usize;
            position += size;
        }
        let unicode = flags & 0x80 != 0;
        // StringData 按这个固定顺序出现，只处理实际置位的那些。
        for mask in [0x4u32, 0x8, 0x10, 0x20, 0x40] {
            if flags & mask == 0 {
                continue;
            }
            let count = u16::from_le_bytes([bytes[position], bytes[position + 1]]) as usize;
            position += 2;
            let length = if unicode { count * 2 } else { count };
            if mask == 0x40 {
                return decode_string(bytes, position, length, unicode);
            }
            position += length;
        }
        panic!("这个 .lnk 里没有 ICON_LOCATION 字段");
    }

    fn decode_string(bytes: &[u8], position: usize, length: usize, unicode: bool) -> String {
        let slice = &bytes[position..position + length];
        if !unicode {
            return String::from_utf8_lossy(slice).into_owned();
        }
        let mut units = Vec::with_capacity(length / 2);
        let mut index = 0;
        while index + 1 < length {
            units.push(u16::from_le_bytes([slice[index], slice[index + 1]]));
            index += 2;
        }
        String::from_utf16_lossy(&units)
    }

    /// 快捷方式不该再散出额外的图标文件。
    #[test]
    fn install_does_not_leave_a_loose_icon_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        // 造一个上一版留下的 sync-gear.ico，install 应该把它清掉
        let legacy = dir.path().join(LEGACY_ICON_FILE);
        fs::write(&legacy, b"stale").unwrap();
        fs::write(dir.path().join(SHORTCUT_NAME), b"stale").unwrap();

        let message = install(dir.path()).unwrap();
        assert!(!legacy.exists(), "上一版的 sync-gear.ico 没有被清理");
        assert!(message.contains("不需要额外的 .ico 文件"));
        assert!(dir.path().join(SHORTCUT_NAME).is_file());
    }
}
