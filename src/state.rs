use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// 一个被登记进本设备同步列表的文件（§6）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SyncFile {
    pub path: String,
}

/// `.rime-sync/sync-list.json`：本设备同步列表 + 排除集（§2、§6）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SyncList {
    #[serde(default)]
    pub files: Vec<SyncFile>,
    /// 明确排除、不再搬运与合并的文件（移除同步项时写入）。
    #[serde(default)]
    pub excluded: Vec<String>,
}

fn normalized_sync_path(value: &str) -> Option<String> {
    crate::paths::validate_sync_file_path(value)
        .ok()
        .map(|path| {
            Path::new(&path)
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        })
}

fn same_sync_path(left: &str, right: &str) -> bool {
    normalized_sync_path(left)
        .is_some_and(|left| normalized_sync_path(right).as_ref() == Some(&left))
}

impl SyncList {
    pub fn load(path: &Path) -> Result<Self> {
        load_json(path)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        save_json(path, self)
    }

    pub fn contains(&self, relative: &str) -> bool {
        self.files.iter().any(|file| file.path == relative)
    }

    /// 比较标准化后的相对路径，兼容旧配置中的 Windows 分隔符。
    pub fn is_excluded(&self, relative: &str) -> bool {
        self.excluded
            .iter()
            .any(|item| same_sync_path(item, relative))
    }

    pub fn register(&mut self, relative: &str) {
        self.excluded.retain(|item| !same_sync_path(item, relative));
        if !self.contains(relative) {
            self.files.push(SyncFile {
                path: relative.to_owned(),
            });
        }
    }

    pub fn remove(&mut self, relative: &str) {
        self.files.retain(|file| file.path != relative);
        if !self.excluded.iter().any(|item| item == relative) {
            self.excluded.push(relative.to_owned());
        }
    }
}

/// 三向判断用的基线：某个文件上次同步后的内容指纹（§5.1）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IndexEntry {
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtime: i64,
}

/// 远端 `Others/` 条目的上次观测结果，用于粗筛是否需要下载（§3）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RemoteMark {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// `.rime-sync/sync-index.json`：全部本地私有基线。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SyncIndex {
    /// 本地源文件基线与 `Others/` 副本基线（三向判断）。
    #[serde(default)]
    pub files: BTreeMap<String, IndexEntry>,
    /// 手动同步已写入 Others、尚未获远端确认的源文件状态；后台只凭此确认基线。
    #[serde(default)]
    pub pending_files: BTreeMap<String, IndexEntry>,
    /// `Others/` 中每个文件的远端观测结果。
    #[serde(default)]
    pub remote: BTreeMap<String, RemoteMark>,
    /// 其他设备快照目录：`<ID>` → 文件名 → sha256。
    #[serde(default)]
    pub snapshots: BTreeMap<String, BTreeMap<String, String>>,
    /// 已报告过的 newest 冲突：`路径` → 冲突时远端内容的 sha256。
    #[serde(default)]
    pub conflicts: BTreeMap<String, String>,
}

impl SyncIndex {
    pub fn load(path: &Path) -> Result<Self> {
        load_json(path)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        save_json(path, self)
    }
}

/// `<ID>/.index.json` 的条目：设备快照的增量清单（§2.1）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SnapshotEntry {
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtime: i64,
}

/// `<ID>/.index.json`：单写者（该 ID 的持有设备）维护的快照清单。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SnapshotIndex {
    #[serde(default)]
    pub installation_id: String,
    #[serde(default)]
    pub generated_at: String,
    #[serde(default)]
    pub files: BTreeMap<String, SnapshotEntry>,
}

impl SnapshotIndex {
    pub fn load(path: &Path) -> Result<Self> {
        load_json(path)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        save_json(path, self)
    }
}

fn load_json<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de> + Default,
{
    if !path.exists() {
        return Ok(T::default());
    }
    let text = fs::read_to_string(path).with_context(|| format!("读取 {} 失败", path.display()))?;
    if text.trim().is_empty() {
        return Ok(T::default());
    }
    serde_json::from_str(&text).with_context(|| format!("解析 {} 失败", path.display()))
}

fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(value)?;
    // 先写临时文件再改名，避免崩溃留下半个 JSON。
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, text).with_context(|| format!("写入 {} 失败", temp.display()))?;
    fs::rename(&temp, path).with_context(|| format!("替换 {} 失败", path.display()))?;
    Ok(())
}

/// 本地互斥锁：后台轮次与手动同步不能并发（§4.2、§10 风险 3）。
pub struct LocalLock {
    file: fs::File,
    path: PathBuf,
}

impl LocalLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        use fs2::FileExt;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("打开同步锁 {} 失败", path.display()))?;
        file.try_lock_exclusive().map_err(|_| {
            anyhow::anyhow!("另一个同步实例正在运行（或多个程序实例同时打开了同一个 RIME 目录）")
        })?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }
}

impl Drop for LocalLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
        let _ = &self.path;
    }
}

/// 迁移 v0.2.5 的 INI 同步列表（`union`/`newest` 标志已被 §5 的类型规则取代）。
pub fn migrate_legacy_sync_files(entries: &[String]) -> SyncList {
    let mut list = SyncList::default();
    for entry in entries {
        let path = entry
            .rsplit_once("/union/")
            .or_else(|| entry.rsplit_once("/newest/"))
            .map(|(path, _)| path)
            .unwrap_or(entry.as_str());
        let Ok(relative) = crate::paths::validate_sync_file_path(path) else {
            continue;
        };
        list.register(&relative);
    }
    list
}

/// 校验一个快照文件是否允许被搬运（拒绝路径穿越）。
pub fn safe_snapshot_name(name: &str) -> Result<()> {
    let reserved = matches!(
        name.split('.')
            .next()
            .unwrap_or(name)
            .to_ascii_uppercase()
            .as_str(),
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
    );
    if name.is_empty()
        || name.contains(['/', '\\', ':', '<', '>', '"', '|', '?', '*'])
        || name.chars().any(char::is_control)
        || name.ends_with(['.', ' '])
        || name == "."
        || name == ".."
        || reserved
    {
        bail!("远端返回了非法的快照文件名: {name}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_list_register_and_remove_moves_to_excluded() {
        let mut list = SyncList::default();
        list.register("custom_phrase.txt");
        assert!(list.contains("custom_phrase.txt"));
        list.remove("custom_phrase.txt");
        assert!(!list.contains("custom_phrase.txt"));
        assert_eq!(list.excluded, vec!["custom_phrase.txt".to_owned()]);
    }

    #[test]
    fn sync_list_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync-list.json");
        let mut list = SyncList::default();
        list.register("my_words.dict.yaml");
        list.save(&path).unwrap();
        assert_eq!(SyncList::load(&path).unwrap().files.len(), 1);
    }

    #[test]
    fn excluded_paths_match_normalized_separators() {
        let mut list = SyncList::default();
        list.remove(r"custom\phrases.txt");
        assert!(list.is_excluded("custom/phrases.txt"));
        assert!(list.is_excluded("custom//phrases.txt"));
        assert!(!list.is_excluded("another/phrases.txt"));
        assert!(!list.is_excluded("../custom/phrases.txt"));
        list.register("custom/phrases.txt");
        assert!(!list.is_excluded("custom/phrases.txt"));
    }

    #[test]
    fn legacy_ini_entries_are_migrated_to_paths_only() {
        let list = migrate_legacy_sync_files(&[
            "default.custom.yaml/newest/N".into(),
            "custom/a.dict.yaml/union/Y".into(),
            "../escape.txt/union/Y".into(),
        ]);
        assert_eq!(list.files.len(), 2);
        assert_eq!(list.files[0].path, "default.custom.yaml");
        assert_eq!(list.files[1].path, "custom/a.dict.yaml");
    }

    #[test]
    fn local_lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync.lock");
        let first = LocalLock::acquire(&path).unwrap();
        assert!(LocalLock::acquire(&path).is_err());
        drop(first);
        assert!(LocalLock::acquire(&path).is_ok());
    }

    #[test]
    fn snapshot_names_reject_windows_prefixes_streams_and_special_files() {
        for name in [
            "C:evil",
            "a.userdb.txt:stream",
            "NUL",
            "CON.userdb.txt",
            "COM1.txt",
            "a/../b",
            "a\\b",
            "bad.",
            "bad ",
            "a\0b",
        ] {
            assert!(safe_snapshot_name(name).is_err(), "accepted {name:?}");
        }
        assert!(safe_snapshot_name("rime_ice.userdb.txt").is_ok());
    }
}
