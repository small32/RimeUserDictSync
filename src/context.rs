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
    /// 能力探测所对应的连接配置摘要，不包含可还原的凭据。
    #[serde(default)]
    pub capabilities_key: String,
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
        crate::hash::write_atomic(path, &text)
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
            // 这里只给一句能立刻照做的话：扫过的候选明细由调用方写进日志
            // （见 `platform::attach_rime_diagnostics`），顶栏一行字放不下也用不着。
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
        // 构建期间也会回写身份与清单，必须与设备设置、同步轮次使用同一把锁。
        let _lock = crate::state::LocalLock::acquire(
            &rime_dir.join(paths::META_DIR_NAME).join(paths::LOCK_FILE),
        )?;
        let mut notices = Vec::new();
        // 自动探测结果要交代清楚：以 root / sudo 运行时 HOME 是 /root，
        // 而候选横跨 /home，最终选中的很可能是 /home/<user> 下的那套。
        if settings.user_data_dir.trim().is_empty()
            && let Some(hint) = crate::platform::rime_dir_origin_hint(&rime_dir)
        {
            notices.push(hint);
        }

        let mut device = if device_path.is_file() {
            DeviceFile::load(&device_path)?
        } else {
            DeviceFile::default()
        };
        if device.capabilities_key != capabilities_key(settings) {
            device.capabilities = Capabilities::default();
            device.capabilities_key.clear();
        }

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

/// 能力缓存必须绑定到实际连接，手工修改 INI 后也不能沿用旧服务端语义。
pub fn capabilities_key(settings: &Settings) -> String {
    crate::hash::sha256_hex(
        format!(
            "{}\0{}\0{}",
            settings.webdav_url.trim(),
            settings.username.trim(),
            settings.password
        )
        .as_bytes(),
    )
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
        // 先完整保存镜像，不能因 userdb 不允许进入 Others 而删掉唯一快照。
        let backup = layout
            .meta
            .join("legacy-backup")
            .join(format!("WebDAV-{}", identity::new_path_token(12)));
        copy_tree(&local_legacy, &backup, false)?;
        let copied = copy_tree(&local_legacy, &layout.others, true)?;
        if copied > 0 {
            notices.push(format!(
                "已把旧版 Sync/WebDAV/ 中的 {copied} 个文件迁移到 Sync/Others/。"
            ));
        }
        fs::remove_dir_all(&local_legacy)?;
        notices.push(format!(
            "旧版 Sync/WebDAV/ 已迁移，完整备份保存在 {}。",
            backup.display()
        ));
    }
    let local_archive = layout.sync.join(crate::archive::ARCHIVE_NAME);
    if local_archive.is_file() {
        let bytes = fs::read(&local_archive)?;
        backup_legacy_bytes(layout, crate::archive::ARCHIVE_NAME, &bytes)?;
        fs::remove_file(&local_archive)?;
        notices.push("旧版 Sync/webdav-sync.zip 已保存在 .rime-sync/legacy-backup/。".to_owned());
    }

    // 远端：webdav-sync.zip → Sync/Others/
    let Some(dav) = dav else {
        return Ok(notices);
    };
    if let MkcolOutcome::Created = dav.mkcol(OTHERS_DIR_NAME)? {
        notices.push("已在远端创建 Others/ 共享目录。".to_owned());
    }
    if let Some(bytes) = dav.try_get_bytes(crate::archive::ARCHIVE_NAME)? {
        let backup = backup_legacy_bytes(layout, crate::archive::ARCHIVE_NAME, &bytes)?;
        let staging = backup
            .parent()
            .context("迁移备份缺少父目录")?
            .join("extracted");
        let extracted = crate::archive::extract(&backup, &staging)?;
        let mut all_published = true;
        for relative in &extracted {
            let source = crate::webdav::safe_local(&staging, relative)?;
            all_published &= publish_legacy_file(dav, layout, relative, &fs::read(source)?, log)?;
        }
        if all_published {
            dav.delete(crate::archive::ARCHIVE_NAME)?;
            notices.push(format!(
                "旧版压缩包的 {} 个文件已发布并校验远端 Others/，已删除远端旧包。",
                extracted.len()
            ));
        } else {
            notices.push(
                "旧版压缩包包含 userdb 快照或有冲突的文件，已保留远端原包和本地完整备份。"
                    .to_owned(),
            );
        }
    }

    // 远端：更早版本留在根目录的散文件 → Sync/Others/
    // 逐项发布，失败后原文件仍存在，下次启动可以继续；不能靠 Others 为空判断是否迁移过。
    {
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
            backup_legacy_bytes(layout, &entry.relative, &bytes)?;
            if publish_legacy_file(dav, layout, &entry.relative, &bytes, log)? {
                dav.delete(&entry.relative)?;
                migrated += 1;
            }
        }
        if migrated > 0 {
            notices.push(format!(
                "已从远端根目录迁移 {migrated} 个旧版散文件到 Sync/Others/。"
            ));
        }
    }

    Ok(notices)
}

fn backup_legacy_bytes(layout: &Layout, relative: &str, bytes: &[u8]) -> Result<PathBuf> {
    let root = layout
        .meta
        .join("legacy-backup")
        .join(crate::hash::sha256_hex(bytes));
    let target = crate::webdav::safe_local(&root, relative)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&target, bytes)?;
    Ok(target)
}

/// true 表示旧来源可以删除；无法合并的配置或 userdb 继续保留原来源。
fn publish_legacy_file(
    dav: &WebDav,
    layout: &Layout,
    relative: &str,
    bytes: &[u8],
    log: &dyn Fn(&str),
) -> Result<bool> {
    let relative = paths::validate_sync_file_path(relative)?;
    if paths::is_private_in_others(&relative) {
        return Ok(true);
    }
    if paths::is_forbidden_in_others(&relative) || paths::is_binary_scheme_resource(&relative) {
        log(&format!(
            "保留旧远端 {relative}：快照或二进制资源不迁入 Others/，本地已有完整备份。"
        ));
        return Ok(false);
    }
    let target = crate::webdav::safe_local(&layout.others, &relative)?;
    let remote = format!("{OTHERS_DIR_NAME}/{relative}");
    let existing = dav.try_get_bytes_with_etag(&remote)?;
    let mut merged = bytes.to_vec();
    let mode = crate::merge::MergeMode::for_path(&relative)?;
    for other in [
        target.is_file().then(|| fs::read(&target)).transpose()?,
        existing.as_ref().map(|(bytes, _)| bytes.clone()),
    ]
    .into_iter()
    .flatten()
    {
        if merged == other {
            continue;
        }
        let Some(text) = crate::merge::merge_text_for_path(
            &relative,
            mode,
            std::str::from_utf8(&merged)?,
            std::str::from_utf8(&other)?,
        ) else {
            log(&format!(
                "保留旧远端 {relative}：Others/ 已有不同配置，请手动确认后再移除旧来源。"
            ));
            return Ok(false);
        };
        merged = text.into_bytes();
    }
    let outcome = if let Some((_, etag)) = &existing {
        dav.put_bytes(&remote, merged.clone(), etag.as_deref())?
    } else {
        dav.put_bytes_if_absent(&remote, merged.clone())?
    };
    if matches!(outcome, crate::webdav::PutOutcome::PreconditionFailed) {
        bail!("迁移期间 {remote} 已被其它设备更新，旧来源已保留，下次启动重试");
    }
    if dav.get_bytes(&remote)? != merged {
        bail!("迁移后的 {remote} 校验不一致，旧来源已保留");
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(target, merged)?;
    log(&format!("已发布并校验旧文件 {relative} → 远端 {remote}。"));
    Ok(true)
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

    /// 界面顶栏只有一行：这里必须只给「照做就行」的那句，
    /// 候选明细交给 `platform::attach_rime_diagnostics` 写日志。
    #[test]
    fn missing_rime_dir_error_stays_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = Settings {
            user_data_dir: dir.path().join("no-such-rime").display().to_string(),
            ..Default::default()
        };
        let ini = dir.path().join("RimeUserDictSync.ini");
        let Err(error) = Context::build(dir.path(), &ini, &mut settings) else {
            panic!("目录不存在时必须报错");
        };
        let error = error.to_string();
        assert!(error.starts_with("找不到 RIME 用户目录"));
        assert!(error.contains("请点击「指定 RIME 用户词库」"));
        assert!(!error.contains("已扫描的候选目录"), "明细不该进这条消息");
        assert!(!error.contains('\n'), "必须是一行");
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
        fs::write(legacy.join("only.userdb.txt"), "唯一快照").unwrap();

        let notices = migrate_legacy_layout(&layout, None, &|_| {}).unwrap();

        assert!(!legacy.exists());
        assert!(layout.others.join("custom_phrase.txt").is_file());
        assert!(!layout.others.join("installation.yaml").exists());
        assert!(!layout.others.join("only.userdb.txt").exists());
        assert!(
            walkdir::WalkDir::new(layout.meta.join("legacy-backup"))
                .into_iter()
                .filter_map(Result::ok)
                .any(|entry| entry.file_name() == "only.userdb.txt")
        );
        assert!(notices.iter().any(|notice| notice.contains("Sync/WebDAV/")));
    }

    struct LegacyServer {
        dav: WebDav,
        files: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>>,
        requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        address: std::net::SocketAddr,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl LegacyServer {
        fn new(name: &str, bytes: Vec<u8>, fail_put: bool) -> Self {
            use std::io::{Read, Write};
            use std::sync::{
                Arc, Mutex,
                atomic::{AtomicBool, Ordering},
            };
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let files = Arc::new(Mutex::new(std::collections::BTreeMap::from([(
                format!("/sync/{name}"),
                bytes,
            )])));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let (server_files, server_requests, server_stop) =
                (files.clone(), requests.clone(), stop.clone());
            let thread = std::thread::spawn(move || {
                let mut directories = std::collections::HashSet::new();
                for stream in listener.incoming() {
                    if server_stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let mut stream = stream.unwrap();
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                        .unwrap();
                    let mut header = Vec::new();
                    let mut byte = [0u8; 1];
                    while !header.ends_with(b"\r\n\r\n") {
                        stream.read_exact(&mut byte).unwrap();
                        header.push(byte[0]);
                    }
                    let header = String::from_utf8(header).unwrap();
                    let mut request = header.lines().next().unwrap().split_whitespace();
                    let (method, path) = (request.next().unwrap(), request.next().unwrap());
                    server_requests
                        .lock()
                        .unwrap()
                        .push(format!("{method} {path}"));
                    let length = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    let mut body = vec![0; length];
                    stream.read_exact(&mut body).unwrap();
                    let mut files = server_files.lock().unwrap();
                    let (status, bytes) = match method {
                        "MKCOL" => (
                            if directories.insert(path.to_owned()) {
                                201
                            } else {
                                405
                            },
                            Vec::new(),
                        ),
                        "GET" => files
                            .get(path)
                            .map(|value| (200, value.clone()))
                            .unwrap_or((404, Vec::new())),
                        "PUT" if fail_put => (500, Vec::new()),
                        "PUT" => {
                            files.insert(path.to_owned(), body);
                            (201, Vec::new())
                        }
                        "DELETE" => {
                            files.remove(path);
                            directories.remove(path);
                            (204, Vec::new())
                        }
                        "PROPFIND" => {
                            let depth_zero = header.lines().any(|line| {
                                line.split_once(':').is_some_and(|(name, value)| {
                                    name.eq_ignore_ascii_case("depth") && value.trim() == "0"
                                })
                            });
                            let requested = path.trim_end_matches('/');
                            let prefix = format!("{requested}/");
                            let visible = |name: &str| {
                                let name = name.trim_end_matches('/');
                                name == requested
                                    || (!depth_zero
                                        && name.strip_prefix(&prefix).is_some_and(|child| {
                                            !child.is_empty() && !child.contains('/')
                                        }))
                            };
                            let mut entries = String::new();
                            for name in directories.iter().filter(|name| visible(name)) {
                                entries.push_str(&format!("<d:response><d:href>{name}</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"));
                            }
                            for name in files.keys().filter(|name| visible(name)) {
                                entries.push_str(&format!("<d:response><d:href>{name}</d:href><d:propstat><d:prop><d:resourcetype/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"));
                            }
                            (
                                207,
                                format!(
                                    "<d:multistatus xmlns:d=\"DAV:\">{entries}</d:multistatus>"
                                )
                                .into_bytes(),
                            )
                        }
                        _ => (500, Vec::new()),
                    };
                    let response = format!(
                        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    stream.write_all(response.as_bytes()).unwrap();
                    stream.write_all(&bytes).unwrap();
                }
            });
            Self {
                dav: WebDav::new(&format!("http://{address}/sync/"), "user", "pass").unwrap(),
                files,
                requests,
                stop,
                address,
                thread: Some(thread),
            }
        }
    }

    impl Drop for LegacyServer {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = std::net::TcpStream::connect(self.address);
            self.thread.take().unwrap().join().unwrap();
        }
    }

    #[test]
    fn legacy_source_is_deleted_only_after_remote_publication_is_verified() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path(), "pc01");
        layout.ensure_dirs().unwrap();
        let server = LegacyServer::new("custom_phrase.txt", b"hello\tcode\n".to_vec(), false);
        migrate_legacy_layout(&layout, Some(&server.dav), &|_| {}).unwrap();
        let files = server.files.lock().unwrap();
        assert!(!files.contains_key("/sync/custom_phrase.txt"));
        assert_eq!(
            files.get("/sync/Others/custom_phrase.txt").unwrap(),
            b"hello\tcode\n"
        );
        drop(files);
        let requests = server.requests.lock().unwrap();
        let verified = requests
            .iter()
            .rposition(|request| request == "GET /sync/Others/custom_phrase.txt")
            .unwrap();
        let deleted = requests
            .iter()
            .position(|request| request == "DELETE /sync/custom_phrase.txt")
            .unwrap();
        assert!(verified < deleted);
    }

    #[test]
    fn failed_legacy_publication_keeps_the_remote_source() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path(), "pc01");
        layout.ensure_dirs().unwrap();
        let server = LegacyServer::new("custom_phrase.txt", b"hello\tcode\n".to_vec(), true);
        assert!(migrate_legacy_layout(&layout, Some(&server.dav), &|_| {}).is_err());
        assert!(
            server
                .files
                .lock()
                .unwrap()
                .contains_key("/sync/custom_phrase.txt")
        );
        assert!(
            !server
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request == "DELETE /sync/custom_phrase.txt")
        );
    }

    #[test]
    fn legacy_remote_snapshot_is_backed_up_and_never_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path(), "pc01");
        layout.ensure_dirs().unwrap();
        let server = LegacyServer::new("only.userdb.txt", b"snapshot".to_vec(), false);
        migrate_legacy_layout(&layout, Some(&server.dav), &|_| {}).unwrap();
        assert!(
            server
                .files
                .lock()
                .unwrap()
                .contains_key("/sync/only.userdb.txt")
        );
        assert!(!layout.others.join("only.userdb.txt").exists());
    }

    #[test]
    fn owner_lock_is_exclusive_and_released_on_drop() {
        let server = LegacyServer::new("custom_phrase.txt", Vec::new(), false);
        let lock = identity::OwnerLock::acquire(&server.dav, "pc01").unwrap();
        assert!(identity::OwnerLock::acquire(&server.dav, "pc01").is_err());
        drop(lock);
        assert!(identity::OwnerLock::acquire(&server.dav, "pc01").is_ok());
    }

    #[test]
    fn archive_migration_publishes_shared_files_but_retains_unique_snapshot_source() {
        use std::io::Write;
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        archive.start_file("custom_phrase.txt", options).unwrap();
        archive.write_all(b"hello\tcode\n").unwrap();
        archive.start_file("only.userdb.txt", options).unwrap();
        archive.write_all(b"unique snapshot").unwrap();
        let bytes = archive.finish().unwrap().into_inner();
        let server = LegacyServer::new(crate::archive::ARCHIVE_NAME, bytes, false);
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path(), "pc01");
        layout.ensure_dirs().unwrap();
        migrate_legacy_layout(&layout, Some(&server.dav), &|_| {}).unwrap();
        let files = server.files.lock().unwrap();
        assert!(files.contains_key("/sync/webdav-sync.zip"));
        assert_eq!(
            files.get("/sync/Others/custom_phrase.txt").unwrap(),
            b"hello\tcode\n"
        );
        assert!(!files.contains_key("/sync/Others/only.userdb.txt"));
    }

    #[test]
    fn context_build_respects_device_settings_lock_without_rewriting_identity() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path(), "pc01");
        layout.ensure_dirs().unwrap();
        fs::write(layout.installation_file(), "installation_id: pc01\n").unwrap();
        let device = DeviceFile {
            installation_id: "pc01".into(),
            session_token: "original".into(),
            ..Default::default()
        };
        device.save(&layout.device_file()).unwrap();
        let before = fs::read(layout.device_file()).unwrap();
        let _lock = crate::state::LocalLock::acquire(&layout.lock_file()).unwrap();
        let mut settings = Settings {
            user_data_dir: dir.path().to_string_lossy().into(),
            webdav_url: "https://example.invalid/".into(),
            username: "user".into(),
            ..Default::default()
        };
        assert!(
            Context::build(dir.path(), &dir.path().join("settings.ini"), &mut settings).is_err()
        );
        assert_eq!(fs::read(layout.device_file()).unwrap(), before);
    }

    #[test]
    fn capabilities_cache_is_invalidated_after_direct_connection_changes() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path(), "pc01");
        layout.ensure_dirs().unwrap();
        fs::write(layout.installation_file(), "installation_id: pc01\n").unwrap();
        let mut settings = Settings {
            user_data_dir: dir.path().to_string_lossy().into(),
            webdav_url: "https://example.invalid/".into(),
            username: "user".into(),
            ..Default::default()
        };
        let capabilities = Capabilities {
            if_match: true,
            etag: true,
            ..Default::default()
        };
        let device = DeviceFile {
            installation_id: "pc01".into(),
            session_token: "token".into(),
            capabilities,
            capabilities_key: capabilities_key(&settings),
            ..Default::default()
        };
        device.save(&layout.device_file()).unwrap();
        let ini = dir.path().join("settings.ini");
        assert_eq!(
            Context::build(dir.path(), &ini, &mut settings)
                .unwrap()
                .device
                .capabilities,
            capabilities
        );
        settings.password = "changed credentials".into();
        let rebuilt = Context::build(dir.path(), &ini, &mut settings).unwrap();
        assert_eq!(rebuilt.device.capabilities, Capabilities::default());
        assert!(rebuilt.device.capabilities_key.is_empty());
    }
}
