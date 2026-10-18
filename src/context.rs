use crate::{
    config::{self, Settings},
    identity::{self, PasswordHash},
    paths::{self, Layout, OTHERS_DIR_NAME, SYNC_DIR_NAME},
    state::{SyncIndex, SyncList, migrate_legacy_sync_files},
    webdav::{Capabilities, MkcolOutcome, WebDav},
};
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// `.rime-sync/device.json`：设备配置的**权威副本**（§2、§10 风险 17）。
///
/// `installation.yaml` 是可见副本（文本编辑器可看、可随配置备份），
/// 但它会被 librime 重写并可能丢掉 `Password` 等字段，所以启动时以本文件为准回写。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DeviceFile {
    #[serde(default)]
    pub installation_id: String,
    #[serde(default)]
    pub sync_dir: String,
    /// 密码派生串（§7.4），不是明文。
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub machine_fp: String,
    #[serde(default)]
    pub host: String,
    /// `owner.json` 里 session_token 的本地副本，每轮用于检测"此 ID 已被其他设备接管"（§7.5）。
    #[serde(default)]
    pub session_token: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub last_sync: String,
    /// 服务端能力探测结果（§9）。
    #[serde(default)]
    pub capabilities: Capabilities,
}

impl DeviceFile {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("读取 {} 失败", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("解析 {} 失败", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        fs::write(path, text).with_context(|| format!("写入 {} 失败", path.display()))
    }

    pub fn password_hash(&self) -> Option<PasswordHash> {
        PasswordHash::parse_storage(&self.password)
    }

    pub fn has_session(&self) -> bool {
        !self.session_token.trim().is_empty()
    }
}

/// 一次同步操作所需的全部上下文。
pub struct Context {
    pub base_dir: PathBuf,
    pub layout: Layout,
    pub settings: Settings,
    pub device: DeviceFile,
    pub dav: WebDav,
    /// 启动阶段收集到的提示，由 UI 显示一次。
    notices: Vec<String>,
}

/// 尚未完成设备设置时的提示（UI 据此打开设置向导）。
pub const NEEDS_DEVICE_SETUP: &str =
    "尚未完成设备设置：请先指定 RIME 用户词库并设置设备 ID 与密码。";

impl Context {
    /// 构建上下文：解析目录、校验 `installation.yaml` 与 `device.json`、
    /// 回写缺失字段、准备 `Sync/` 布局。
    pub fn build(base_dir: &Path, ini: &Path, settings: &mut Settings) -> Result<Self> {
        let rime_dir = resolve_rime_dir(settings);
        if !rime_dir.is_dir() {
            bail!(
                "找不到 RIME 用户目录 {}，请点击「指定 RIME 用户词库」",
                rime_dir.display()
            );
        }
        let installation = rime_dir.join("installation.yaml");
        if !installation.is_file() {
            bail!(
                "{} 中没有 installation.yaml，请选择真正的 RIME 用户目录",
                rime_dir.display()
            );
        }
        if !settings.webdav_ready() {
            bail!("尚未配置 WebDAV 地址或用户名，请先在「WebDAV 设置」中填写并测试连接");
        }

        let sync_dir = rime_dir.join(SYNC_DIR_NAME);
        let device_path = rime_dir.join(paths::META_DIR_NAME).join(paths::DEVICE_FILE);
        let mut notices = Vec::new();

        let mut device = if device_path.is_file() {
            DeviceFile::load(&device_path)?
        } else {
            DeviceFile::default()
        };

        // 设备 ID：以 device.json 为准；首次运行时从 installation.yaml 继承。
        if device.installation_id.trim().is_empty() {
            match config::yaml_scalar_opt(&installation, "installation_id") {
                Some(id) if paths::validate_installation_id(&id).is_ok() => {
                    device.installation_id = id;
                }
                _ => bail!("{NEEDS_DEVICE_SETUP}"),
            }
        }
        paths::validate_installation_id(&device.installation_id)?;
        // 没有 session_token 说明设备设置还没走完（§7.1）；
        // 缺了它就每轮都无法校验"此 ID 是否已被接管"。
        if !device.has_session() {
            device.save(&device_path)?;
            bail!("{NEEDS_DEVICE_SETUP}");
        }

        if device.machine_fp.trim().is_empty() {
            device.machine_fp = identity::new_machine_fp();
        }
        if device.host.trim().is_empty() {
            device.host = identity::host_name();
        }
        if device.created_at.trim().is_empty() {
            device.created_at = identity::now_stamp();
        }
        device.sync_dir = sync_dir.to_string_lossy().into_owned();

        // 可见副本缺失的密码派生串：从 installation.yaml 继承一次。
        if device.password.trim().is_empty()
            && let Some(stored) = config::yaml_scalar_opt(&installation, "Password")
            && PasswordHash::parse_storage(&stored).is_some()
        {
            device.password = stored;
        }

        device.save(&device_path)?;
        notices.extend(reconcile_installation_yaml(&installation, &device)?);

        let layout = Layout::new(&rime_dir, &device.installation_id);
        layout.ensure_dirs()?;
        let quarantined = paths::quarantine_forbidden_others(&layout)?;
        if !quarantined.is_empty() {
            notices.push(format!(
                "Others/ 里混进了 userdb 快照（{}），已挪到 {}；librime 会把 Others/ 当成一台设备合并它，必须移出去。",
                quarantined.join("、"),
                layout.meta.join("quarantine").display()
            ));
        }
        notices.extend(paths::audit_sync_root(&layout));

        let mut settings_cleanup = settings.clone();
        ensure_sync_list(&layout, &mut settings_cleanup, ini)?;
        if settings_cleanup.legacy_sync_files != settings.legacy_sync_files {
            *settings = settings_cleanup;
        }

        let dav = WebDav::new(
            settings.webdav_url.trim(),
            settings.username.trim(),
            &settings.password,
        )?;

        Ok(Self {
            base_dir: base_dir.to_path_buf(),
            layout,
            settings: settings.clone(),
            device,
            dav,
            notices,
        })
    }

    /// 启动阶段收集到的提示（由 UI 记录一次，避免后台轮次反复打印）。
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    pub fn load_list(&self) -> Result<SyncList> {
        SyncList::load(&self.layout.list_file())
    }

    pub fn save_list(&self, list: &SyncList) -> Result<()> {
        list.save(&self.layout.list_file())
    }

    pub fn load_index(&self) -> Result<SyncIndex> {
        SyncIndex::load(&self.layout.index_file())
    }

    pub fn save_index(&self, index: &SyncIndex) -> Result<()> {
        index.save(&self.layout.index_file())
    }

    /// 测试用的最小上下文（不会发起任何网络请求）。
    #[cfg(test)]
    pub fn for_test(root: &Path, id: &str) -> Self {
        let layout = Layout::new(root.join("Rime"), id);
        Self {
            base_dir: root.to_path_buf(),
            layout,
            settings: Settings::default(),
            device: DeviceFile {
                installation_id: id.to_owned(),
                sync_dir: String::new(),
                session_token: "test-token".to_owned(),
                ..Default::default()
            },
            dav: WebDav::new("https://example.invalid/", "user", "pass")
                .expect("测试用 WebDAV 客户端"),
            notices: Vec::new(),
        }
    }
}

/// 解析 RIME 用户目录（配置值优先，其次平台默认）。
pub fn resolve_rime_dir(settings: &Settings) -> PathBuf {
    if settings.user_data_dir.trim().is_empty() {
        crate::platform::default_user_dir()
    } else {
        PathBuf::from(crate::platform::expand_env(&settings.user_data_dir))
    }
}

/// 双向校验 `installation.yaml` 与 `device.json`，不一致以 `device.json` 为准回写（§2）。
pub fn reconcile_installation_yaml(
    installation: &Path,
    device: &DeviceFile,
) -> Result<Vec<String>> {
    let mut notices = Vec::new();
    let id = config::yaml_scalar_opt(installation, "installation_id");
    if id.as_deref() != Some(device.installation_id.as_str()) {
        config::set_yaml_scalar(installation, "installation_id", &device.installation_id)?;
        notices.push(format!(
            "installation.yaml 的 installation_id 与 device.json 不一致，已按权威副本回写为 {}。",
            device.installation_id
        ));
    }
    let sync_dir = config::yaml_scalar_opt(installation, "sync_dir");
    if sync_dir.as_deref() != Some(device.sync_dir.as_str()) {
        config::set_yaml_scalar(installation, "sync_dir", &device.sync_dir)?;
        notices.push(format!(
            "installation.yaml 的 sync_dir 已被设置为 {}。",
            device.sync_dir
        ));
    }
    if !device.password.trim().is_empty() {
        let stored = config::yaml_scalar_opt(installation, "Password");
        if stored.as_deref() != Some(device.password.as_str()) {
            config::set_yaml_scalar(installation, "Password", &device.password)?;
            notices.push(
                "installation.yaml 的 Password 已按 device.json 回写（保存的是 KDF 派生串，不是明文）。"
                    .to_owned(),
            );
        }
    }
    Ok(notices)
}

/// 首次运行时把 v0.2.5 的 INI 同步列表迁移进 `sync-list.json`。
fn ensure_sync_list(layout: &Layout, settings: &mut Settings, ini: &Path) -> Result<()> {
    let path = layout.list_file();
    if path.is_file() {
        // 迁移已完成：清掉 INI 里的旧字段，避免两处真相。
        if !settings.legacy_sync_files.is_empty() {
            settings.legacy_sync_files.clear();
            let _ = settings.save(ini);
        }
        return Ok(());
    }
    let list = if settings.legacy_sync_files.is_empty() {
        default_sync_list()
    } else {
        migrate_legacy_sync_files(&settings.legacy_sync_files)
    };
    list.save(&path)?;
    if !settings.legacy_sync_files.is_empty() {
        settings.legacy_sync_files.clear();
        let _ = settings.save(ini);
    }
    Ok(())
}

/// §8.2 的默认清单：只勾选用户自己产生/修改过的文件。
pub fn default_sync_list() -> SyncList {
    let mut list = SyncList::default();
    for path in [
        "custom_phrase.txt",
        "my_words.dict.yaml",
        "default.custom.yaml",
    ] {
        list.register(path);
    }
    list
}

/// v0.2.5 及更早版本的目录布局迁移（§11）。
///
/// 旧布局：`Sync/WebDAV/`（远端镜像）+ 可选 `Sync/webdav-sync.zip`。
/// 新布局：`Sync/Others/`（唯一共享目录）+ `Sync/<ID>/`（快照）。
/// `Sync/WebDAV/` **必须**清掉，否则 librime 会把它当成一台设备参与合并。
pub fn migrate_legacy_layout(
    layout: &Layout,
    dav: Option<&WebDav>,
    log: &dyn Fn(&str),
) -> Result<Vec<String>> {
    let mut notices = Vec::new();

    // 本地：Sync/WebDAV/ → Sync/Others/
    let local_legacy = layout.sync.join("WebDAV");
    if local_legacy.is_dir() {
        let copied = copy_tree(&local_legacy, &layout.others, true)?;
        if copied > 0 {
            notices.push(format!(
                "已把旧版 Sync/WebDAV/ 中的 {copied} 个文件迁移到 Sync/Others/。"
            ));
        }
        fs::remove_dir_all(&local_legacy).ok();
        notices.push("已删除旧版 Sync/WebDAV/，避免 librime 把它当成一台设备参与合并。".to_owned());
    }
    let local_archive = layout.sync.join(crate::archive::ARCHIVE_NAME);
    if local_archive.is_file() {
        fs::remove_file(&local_archive).ok();
        notices.push("已删除旧版 Sync/webdav-sync.zip 临时压缩包。".to_owned());
    }

    // 远端：webdav-sync.zip → Sync/Others/
    let Some(dav) = dav else {
        return Ok(notices);
    };
    let others_empty = fs::read_dir(&layout.others)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(true);
    if others_empty && let Some(bytes) = dav.try_get_bytes(crate::archive::ARCHIVE_NAME)? {
        let temp = layout.meta.join(crate::archive::ARCHIVE_NAME);
        fs::create_dir_all(&layout.meta)?;
        fs::write(&temp, &bytes)?;
        let extracted = crate::archive::extract(&temp, &layout.others)?;
        let _ = fs::remove_file(&temp);
        let kept = extracted
            .iter()
            .filter(|name| !name.ends_with("installation.yaml"))
            .count();
        notices.push(format!(
            "已从远端旧版压缩包迁移 {kept} 个文件到 Sync/Others/。"
        ));
        dav.delete(crate::archive::ARCHIVE_NAME)?;
        notices.push("已删除远端旧版 webdav-sync.zip。".to_owned());
    }

    // 远端：更早版本留在根目录的散文件 → Sync/Others/
    // 解包可能刚往 Others/ 写过内容，重新判定一次，避免与刚解包的文件互相覆盖。
    let others_empty = fs::read_dir(&layout.others)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(true);
    if others_empty {
        let loose: Vec<_> = dav
            .list("")?
            .into_iter()
            .filter(|entry| !entry.is_dir)
            .filter(|entry| !entry.relative.ends_with(crate::archive::ARCHIVE_NAME))
            .collect();
        let mut migrated = 0;
        for entry in loose {
            let Some(bytes) = dav.try_get_bytes(&entry.relative)? else {
                continue;
            };
            if entry.relative.ends_with("installation.yaml") {
                let _ = dav.delete(&entry.relative);
                continue;
            }
            let target = crate::webdav::safe_local(&layout.others, &entry.relative)?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&target, &bytes)?;
            log(&format!(
                "迁移远端旧文件 {} → Sync/Others/{}",
                entry.relative, entry.relative
            ));
            dav.delete(&entry.relative)?;
            migrated += 1;
        }
        if migrated > 0 {
            notices.push(format!(
                "已从远端根目录迁移 {migrated} 个旧版散文件到 Sync/Others/。"
            ));
        }
    }

    // 确保远端 Others/ 存在。
    if let MkcolOutcome::Created = dav.mkcol(OTHERS_DIR_NAME)? {
        notices.push("已在远端创建 Others/ 共享目录。".to_owned());
    }
    Ok(notices)
}

/// 把设备配置落到磁盘：`device.json`（权威）+ `installation.yaml`（可见副本）。
pub fn persist_device(rime_dir: &Path, device: &DeviceFile) -> Result<Vec<String>> {
    let layout = Layout::new(rime_dir, &device.installation_id);
    layout.ensure_dirs()?;
    device.save(&layout.device_file())?;
    reconcile_installation_yaml(&layout.installation_file(), device)
}

fn copy_tree(source: &Path, target: &Path, skip_installation: bool) -> Result<usize> {
    let mut copied = 0;
    for entry in walkdir::WalkDir::new(source) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(source)?;
        // installation.yaml 是旧版镜像的占位文件；*.userdb.txt 则绝不能进 Others/
        // （§2.2 红线），它该待在 Sync/<ID>/ 里由 librime 自己合并。
        if skip_installation
            && (relative
                .file_name()
                .is_some_and(|name| name == "installation.yaml")
                || paths::is_forbidden_in_others(&relative.to_string_lossy()))
        {
            continue;
        }
        let destination = target.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(entry.path(), &destination)?;
        copied += 1;
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, sync_dir: &str) -> DeviceFile {
        DeviceFile {
            installation_id: id.to_owned(),
            sync_dir: sync_dir.to_owned(),
            password: "pbkdf2-sha256$1000$c2FsdA==$aGFzaA==".to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn reconcile_rewrites_fields_missing_from_installation_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("installation.yaml");
        fs::write(&yaml, "installation_id: old\nsync_dir: /tmp/old\n").unwrap();
        let notices =
            reconcile_installation_yaml(&yaml, &device("pc01", "/tmp/rime/Sync")).unwrap();
        assert_eq!(notices.len(), 3);
        assert_eq!(
            config::yaml_scalar_opt(&yaml, "installation_id").as_deref(),
            Some("pc01")
        );
        assert_eq!(
            config::yaml_scalar_opt(&yaml, "sync_dir").as_deref(),
            Some("/tmp/rime/Sync")
        );
        assert_eq!(
            config::yaml_scalar_opt(&yaml, "Password").as_deref(),
            Some("pbkdf2-sha256$1000$c2FsdA==$aGFzaA==")
        );
        // 幂等：第二次不再产生通知。
        assert!(
            reconcile_installation_yaml(&yaml, &device("pc01", "/tmp/rime/Sync"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn default_sync_list_covers_documented_files() {
        let list = default_sync_list();
        assert!(list.contains("custom_phrase.txt"));
        assert!(list.contains("my_words.dict.yaml"));
        assert!(list.contains("default.custom.yaml"));
    }

    #[test]
    fn legacy_webdav_dir_is_moved_into_others_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().join("Rime"), "pc01");
        layout.ensure_dirs().unwrap();
        let legacy = layout.sync.join("WebDAV");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(
            legacy.join("installation.yaml"),
            "installation_id: WebDAV\n",
        )
        .unwrap();
        fs::write(legacy.join("custom_phrase.txt"), "你好\tni hao\t1\n").unwrap();

        let notices = migrate_legacy_layout(&layout, None, &|_| {}).unwrap();

        assert!(!legacy.exists());
        assert!(layout.others.join("custom_phrase.txt").is_file());
        assert!(!layout.others.join("installation.yaml").exists());
        assert!(notices.iter().any(|notice| notice.contains("Sync/WebDAV/")));
    }
}
