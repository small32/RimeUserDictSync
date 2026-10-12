use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

/// `RIME/Sync`：唯一与 WebDAV 同步的目录（§2）。
pub const SYNC_DIR_NAME: &str = "Sync";
/// `Sync/Others`：合并后的公共结果，不分设备 ID。
pub const OTHERS_DIR_NAME: &str = "Others";
/// `RIME/.rime-sync`：工具私有状态，**严禁**放进 `Sync/`（§2、§10 风险 13）。
pub const META_DIR_NAME: &str = ".rime-sync";

pub const DEVICE_FILE: &str = "device.json";
pub const INDEX_FILE: &str = "sync-index.json";
pub const LIST_FILE: &str = "sync-list.json";
pub const LOCK_FILE: &str = "sync.lock";
/// UI 请求后台服务「立刻跑一轮」的标记文件；服务跑完即删。
pub const WAKE_FILE: &str = "wake";

/// 设备快照目录内由 librime / 工具维护的特殊文件名。
pub const OWNER_FILE: &str = "owner.json";
pub const SNAPSHOT_INDEX_FILE: &str = ".index.json";

/// `Others/` 内禁止出现的后缀（§2.2、§10 风险 4）。
const FORBIDDEN_OTHERS_SUFFIX: &str = ".userdb.txt";

/// 设备配置的目录布局（§2）。
#[derive(Clone, Debug)]
pub struct Layout {
    /// RIME 用户目录（`installation.yaml` 所在目录）。
    pub rime: PathBuf,
    /// `RIME/Sync`。
    pub sync: PathBuf,
    /// `RIME/Sync/Others`。
    pub others: PathBuf,
    /// `RIME/.rime-sync`。
    pub meta: PathBuf,
    /// `RIME/Sync/<ID>`：本设备的 userdb 快照目录。
    pub device: PathBuf,
    pub installation_id: String,
}

impl Layout {
    pub fn new(rime: impl Into<PathBuf>, installation_id: &str) -> Self {
        let rime = rime.into();
        let sync = rime.join(SYNC_DIR_NAME);
        let others = sync.join(OTHERS_DIR_NAME);
        let meta = rime.join(META_DIR_NAME);
        let device = sync.join(installation_id);
        Self {
            rime,
            sync,
            others,
            meta,
            device,
            installation_id: installation_id.to_owned(),
        }
    }

    pub fn installation_file(&self) -> PathBuf {
        self.rime.join("installation.yaml")
    }

    pub fn device_file(&self) -> PathBuf {
        self.meta.join(DEVICE_FILE)
    }

    pub fn index_file(&self) -> PathBuf {
        self.meta.join(INDEX_FILE)
    }

    pub fn list_file(&self) -> PathBuf {
        self.meta.join(LIST_FILE)
    }

    pub fn lock_file(&self) -> PathBuf {
        self.meta.join(LOCK_FILE)
    }

    /// 本设备快照目录里的 `.index.json`。
    pub fn snapshot_index_file(&self) -> PathBuf {
        self.device.join(SNAPSHOT_INDEX_FILE)
    }

    /// 创建 `Sync/`、`Others/`、`.rime-sync/` 与设备快照目录。
    pub fn ensure_dirs(&self) -> Result<()> {
        // `Sync/` 必须真实存在，librime 只会遍历既有子目录。
        for dir in [&self.sync, &self.others, &self.meta, &self.device] {
            fs::create_dir_all(dir).with_context(|| format!("创建目录 {} 失败", dir.display()))?;
        }
        Ok(())
    }

    /// 同步文件在 RIME 用户目录中的本地源路径。
    pub fn local_source(&self, relative: &str) -> PathBuf {
        join_relative(&self.rime, relative)
    }

    /// 同步文件在 `Sync/Others/` 中的共享副本路径。
    pub fn others_path(&self, relative: &str) -> PathBuf {
        join_relative(&self.others, relative)
    }
}

/// 把 `/` 分隔的相对路径拼到根目录上，拒绝 `..` 与绝对路径。
pub fn join_relative(root: &Path, relative: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    for part in relative
        .replace('\\', "/")
        .split('/')
        .filter(|part| !part.is_empty())
    {
        out.push(part);
    }
    out
}

/// 设备 ID 必须能安全地同时充当目录名和 URL 段（§7.1）。
///
/// 规则：`^[A-Za-z0-9][A-Za-z0-9_-]{2,31}$`，另加两个额外护栏
/// （Windows 保留设备名、`Others`/`WebDAV` 保留目录名）。
pub fn validate_installation_id(value: &str) -> Result<()> {
    let trimmed = value.trim();
    if trimmed != value {
        bail!("设备 ID 前后不能有空格: {value}");
    }
    let bytes = value.as_bytes();
    if !(3..=32).contains(&bytes.len()) {
        bail!("设备 ID 长度必须是 3~32 个字符: {value}");
    }
    if !bytes[0].is_ascii_alphanumeric() {
        bail!("设备 ID 必须以英文字母或数字开头: {value}");
    }
    if !bytes[1..]
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-')
    {
        bail!("设备 ID 只能包含英文字母、数字、下划线和连字符: {value}");
    }
    if value.eq_ignore_ascii_case(OTHERS_DIR_NAME) {
        bail!("设备 ID 不能是保留目录名 {OTHERS_DIR_NAME}");
    }
    if value.eq_ignore_ascii_case("WebDAV") {
        bail!("设备 ID 不能是旧版保留目录名 WebDAV");
    }
    if is_windows_reserved_name(value) {
        bail!("设备 ID 不能是 Windows 保留设备名: {value}");
    }
    Ok(())
}

/// 大小写不敏感的重复检测：Windows/macOS 文件系统不区分大小写，Linux 区分。
pub fn ensure_id_not_taken<'a, I>(value: &str, existing: I) -> Result<()>
where
    I: IntoIterator<Item = &'a str>,
{
    for other in existing {
        if !other.is_empty() && other.eq_ignore_ascii_case(value) {
            bail!("设备 ID 已被占用（大小写不敏感）: {value}");
        }
    }
    Ok(())
}

fn is_windows_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

/// 校验同步文件路径，并要求文件名是纯 ASCII（§2.2、§10 风险 11）。
pub fn validate_sync_file_path(value: &str) -> Result<String> {
    let normalized = value.replace('\\', "/");
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        bail!("同步文件路径不能为空");
    }
    if trimmed.contains(';') {
        bail!("同步文件路径不能包含分号: {value}");
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        bail!("同步文件路径必须是相对于 RIME 用户目录的相对路径: {value}");
    }
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                let name = name.to_string_lossy();
                if !name.is_ascii() {
                    bail!(
                        "同步文件名必须是纯 ASCII（macOS 的 NFD 与 Windows 的 NFC 表示不一致）: {value}"
                    );
                }
                if name.contains(':') {
                    bail!("同步文件路径不能包含冒号: {value}");
                }
            }
            _ => bail!("同步文件路径不能包含 . 或 .. : {value}"),
        }
    }
    Ok(trimmed.to_owned())
}

/// `Sync/Others/` 中禁止出现的文件（`*.userdb.txt` 会被 librime 当成一台设备参与合并）。
pub fn is_forbidden_in_others(relative: &str) -> bool {
    relative
        .to_ascii_lowercase()
        .ends_with(FORBIDDEN_OTHERS_SUFFIX)
}

/// 方案自带的二进制资源（万象 `.gram`、模型等）：§8.2 明确不同步。
///
/// librime 的 `/sync` 可能把它们一起复制进快照目录，所以无论调用顺序如何，
/// 上传前与 `/sync` 之后都要按这个判断把它们排除掉。
pub fn is_binary_scheme_resource(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".gram", ".bin", ".model", ".zip", ".userdb"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

/// 把 `Others/` 里不该存在的文件挪进 `.rime-sync/quarantine/`。
///
/// §2.2 红线：librime 会把 `Others/` 当成一台"设备"参与合并，而整行并集
/// 又会破坏 `*.userdb.txt` 的负 commits 删除语义。所以它绝不能待在共享目录里，
/// 但也**不能直接删掉**（那可能是用户唯一的一份快照），一律挪走并提示。
pub fn quarantine_forbidden_others(layout: &Layout) -> Result<Vec<String>> {
    let mut moved = Vec::new();
    if !layout.others.is_dir() {
        return Ok(moved);
    }
    for entry in walkdir::WalkDir::new(&layout.others)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(&layout.others)?
            .to_string_lossy()
            .replace('\\', "/");
        if !is_forbidden_in_others(&relative) {
            continue;
        }
        let target = layout.meta.join("quarantine").join(&relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(entry.path(), &target).or_else(|_| {
            fs::copy(entry.path(), &target)?;
            fs::remove_file(entry.path())
        })?;
        moved.push(relative);
    }
    Ok(moved)
}

/// 目录体检：返回需要提示用户的问题列表（§2.2、§10 风险 12/13/14）。
pub fn audit_sync_root(layout: &Layout) -> Vec<String> {
    let mut warnings = Vec::new();
    if let Some(collision) = sync_case_collision(&layout.rime) {
        warnings.push(format!(
            "RIME 用户目录下同时存在 `Sync` 与 `{collision}`；在大小写不敏感的文件系统上它们是同一个目录，请确认 RIME 的 sync_dir 指向它。"
        ));
    }
    for name in [
        DEVICE_FILE,
        INDEX_FILE,
        LIST_FILE,
        ".sync-index",
        ".sync-list",
    ] {
        if layout.sync.join(name).exists() {
            warnings.push(format!(
                "`Sync/{name}` 属于工具私有状态，不应出现在同步目录中；请删除它（正确位置是 `{META_DIR_NAME}/`）。"
            ));
        }
    }
    if let Ok(entries) = fs::read_dir(&layout.others) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_forbidden_in_others(&name) {
                warnings.push(format!(
                    "`Others/{name}` 是 userdb 快照，放进共享目录会破坏删除语义，请从同步列表以外的位置移除。"
                ));
            }
            if !name.is_ascii() {
                warnings.push(format!(
                    "`Others/{name}` 不是纯 ASCII 文件名，跨平台可能出现重名，建议改为英文名。"
                ));
            }
        }
    }
    warnings
}

/// 在 `RIME/` 下寻找与 `Sync` 仅大小写不同的同级目录。
fn sync_case_collision(rime: &Path) -> Option<String> {
    let entries = fs::read_dir(rime).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != SYNC_DIR_NAME && name.eq_ignore_ascii_case(SYNC_DIR_NAME) {
            return Some(name);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installation_id_follows_documented_charset() {
        assert!(validate_installation_id("MacBookAir").is_ok());
        assert!(validate_installation_id("pc-01_work").is_ok());
        assert!(validate_installation_id("ab").is_err());
        assert!(validate_installation_id("1中文机").is_err());
        assert!(validate_installation_id("has space").is_err());
        assert!(validate_installation_id("has.dot").is_err());
        assert!(validate_installation_id("has/slash").is_err());
        assert!(validate_installation_id("_leading").is_err());
        assert!(validate_installation_id("-leading").is_err());
        assert!(validate_installation_id("con").is_err());
        assert!(validate_installation_id("Others").is_err());
        assert!(validate_installation_id("webdav").is_err());
    }

    #[test]
    fn installation_id_duplicate_check_ignores_case() {
        assert!(ensure_id_not_taken("MacBookAir", ["pc", "macbookair"]).is_err());
        assert!(ensure_id_not_taken("MacBookAir", ["pc", "thinkpad"]).is_ok());
    }

    #[test]
    fn sync_file_path_must_be_relative_and_ascii() {
        assert_eq!(
            validate_sync_file_path("cn_dicts/base.dict.yaml").unwrap(),
            "cn_dicts/base.dict.yaml"
        );
        assert!(validate_sync_file_path("../escape.txt").is_err());
        assert!(validate_sync_file_path("/abs.txt").is_err());
        assert!(validate_sync_file_path("中文.txt").is_err());
        assert!(validate_sync_file_path("a;b.txt").is_err());
    }

    #[test]
    fn layout_places_private_state_outside_sync() {
        let layout = Layout::new("Rime", "pc01");
        assert!(layout.index_file().starts_with(&layout.meta));
        assert!(!layout.index_file().starts_with(&layout.sync));
        assert!(layout.device.ends_with("Sync/pc01"));
    }

    #[test]
    fn quarantine_moves_userdb_snapshots_out_of_others() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().join("Rime"), "pc01");
        layout.ensure_dirs().unwrap();
        fs::create_dir_all(layout.others.join("nested")).unwrap();
        fs::write(layout.others.join("rime_ice.userdb.txt"), b"a\tb\t1\n").unwrap();
        fs::write(layout.others.join("nested/x.userdb.txt"), b"c\td\t1\n").unwrap();
        fs::write(layout.others.join("custom_phrase.txt"), b"hello\t1\n").unwrap();

        let moved = quarantine_forbidden_others(&layout).unwrap();

        assert_eq!(moved.len(), 2);
        assert!(!layout.others.join("rime_ice.userdb.txt").exists());
        assert!(!layout.others.join("nested/x.userdb.txt").exists());
        assert!(layout.others.join("custom_phrase.txt").exists());
        assert!(layout.meta.join("quarantine/nested/x.userdb.txt").is_file());
    }
}
