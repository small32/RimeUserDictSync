use crate::{
    context::DeviceFile,
    identity::{self, Owner, PasswordHash, Takeover},
    paths::{self, Layout, SNAPSHOT_INDEX_FILE},
    state::{SnapshotIndex, SyncList},
    webdav::{Capabilities, MkcolOutcome, WebDav},
};
use anyhow::{Context as _, Result, bail};
use std::{fs, path::PathBuf};

/// 设备设置对话框会触发的全部动作（在后台线程执行）。
#[derive(Clone, Debug)]
pub enum DeviceAction {
    /// §7.1 抢占 + 设密码；若 ID 已被占用则退回 §7.2 密码验证接管。
    Claim {
        id: String,
        password: Option<String>,
        skip_password: bool,
        confirm: bool,
    },
    /// §7.3 强制接管（忘密码的兜底），要求重设新密码。
    TakeoverForced {
        id: String,
        new_password: String,
        confirm: bool,
    },
    /// §7.7 修改密码，不轮换 session_token。
    ChangePassword {
        old_password: String,
        new_password: String,
    },
    /// §7.6 注销此设备。
    Retire { password: String },
    /// §7.8 导出设备配置。
    Export,
    /// §7.8 导入设备配置。
    Import { text: String },
}

/// 建立设备身份所需的本地信息（换机器重建时复用同一份指纹）。
#[derive(Clone, Debug)]
pub struct DeviceSeed {
    pub sync_dir: String,
    pub machine_fp: String,
    pub host: String,
    pub app_ver: String,
    pub created_at: String,
    pub capabilities: Capabilities,
    pub capabilities_key: String,
}

impl DeviceSeed {
    pub fn from_device(device: &DeviceFile, sync_dir: impl Into<String>) -> Self {
        Self {
            capabilities: device.capabilities,
            capabilities_key: device.capabilities_key.clone(),
            sync_dir: sync_dir.into(),
            machine_fp: if device.machine_fp.trim().is_empty() {
                identity::new_machine_fp()
            } else {
                device.machine_fp.clone()
            },
            host: if device.host.trim().is_empty() {
                identity::host_name()
            } else {
                device.host.clone()
            },
            app_ver: env!("CARGO_PKG_VERSION").to_owned(),
            created_at: if device.created_at.trim().is_empty() {
                identity::now_stamp()
            } else {
                device.created_at.clone()
            },
        }
    }
}

/// 抢占 ID 的结果（§7.1）。
pub enum Claim {
    /// `MKCOL` 返回 201，ID 归本机所有。
    Claimed(Box<DeviceFile>),
    /// 远端已有此 ID（保证不进行目录覆盖），附带它的 `owner.json`（可能不存在）。
    Occupied(Box<Option<Owner>>),
}

/// 首次设置：抢占 + 设密码（§7.1）。`password` 为 `None` 表示显式跳过设密码。
pub fn claim(dav: &WebDav, id: &str, password: Option<&str>, seed: &DeviceSeed) -> Result<Claim> {
    validate_remote_id(dav, id)?;
    let _owner_lock = identity::OwnerLock::acquire(dav, id)?;
    match dav.mkcol(id)? {
        MkcolOutcome::AlreadyExists => Ok(Claim::Occupied(Box::new(Owner::load(dav, id)?))),
        MkcolOutcome::Created => {
            // Others/ 若不存在则幂等创建。
            let _ = dav.mkcol(paths::OTHERS_DIR_NAME)?;
            let mut device = DeviceFile {
                capabilities: seed.capabilities,
                capabilities_key: seed.capabilities_key.clone(),
                installation_id: id.to_owned(),
                sync_dir: seed.sync_dir.clone(),
                machine_fp: seed.machine_fp.clone(),
                host: seed.host.clone(),
                session_token: identity::new_session_token(),
                created_at: seed.created_at.clone(),
                last_sync: identity::now_stamp(),
                ..Default::default()
            };
            if let Some(password) = password {
                let derived = PasswordHash::derive(password);
                device.password = derived.to_storage();
            }
            let owner = owner_from_device(&device, password.is_some());
            owner.save_if_absent(dav, id)?;
            Ok(Claim::Claimed(Box::new(device)))
        }
    }
}

/// 必须在把 ID 用于本地路径或远端请求之前调用。
pub fn validate_remote_id(dav: &WebDav, id: &str) -> Result<()> {
    paths::validate_installation_id(id)?;
    reject_case_insensitive_duplicate(dav, id)
}

/// §7.1 第 1 步：查重大小写不敏感（Win/macOS 文件系统不敏感，Linux 敏感）。
///
/// 完全同名的目录允许（那正是"远端已有此 ID"的接管场景）；
/// 只有"同名但大小写不同"才必须换 ID，否则在大小写不敏感的系统上会互相踩。
fn reject_case_insensitive_duplicate(dav: &WebDav, id: &str) -> Result<()> {
    let entries = dav.list("")?;
    let others: Vec<&str> = entries
        .iter()
        .filter(|entry| entry.is_dir)
        .map(|entry| entry.relative.trim_end_matches('/'))
        .filter(|name| !name.is_empty() && *name != id)
        .collect();
    paths::ensure_id_not_taken(id, others)
}

fn owner_from_device(device: &DeviceFile, has_password: bool) -> Owner {
    Owner {
        id: device.installation_id.clone(),
        host: device.host.clone(),
        app_ver: env!("CARGO_PKG_VERSION").to_owned(),
        created_at: device.created_at.clone(),
        last_sync: identity::now_stamp(),
        machine_fp: device.machine_fp.clone(),
        session_token: device.session_token.clone(),
        pwd: if has_password {
            device.password_hash()
        } else {
            None
        },
        pwd_set_at: has_password.then(identity::now_stamp),
        taken_over_from: None,
    }
}

/// 远端 `owner.json` 的最后同步距今分钟数；解析失败返回 `None`。
pub fn minutes_since_last_sync(owner: &Owner) -> Option<i64> {
    let last = chrono::NaiveDateTime::parse_from_str(&owner.last_sync, "%Y-%m-%d %H:%M:%S").ok()?;
    let now = chrono::Local::now().naive_local();
    Some((now - last).num_minutes())
}

/// §7.2：远端已有此 ID 且设过密码 → 密码验证。
pub fn verify(owner: &Owner, password: &str) -> Result<()> {
    if !owner.has_password() {
        bail!(
            "该 ID 从未设置密码（老版本创建或他人手工建目录），密码验证不可用；请改用「强制接管」或更换 ID。"
        );
    }
    if !owner.verify_password(password) {
        bail!("密码不正确：此 ID 属于其他设备，必须更换 ID。");
    }
    Ok(())
}

/// §7.2 的「自动复原」第 3 步：覆盖 owner.json，签发新 session_token，保留 pwd。
pub fn rewrite_owner(
    dav: &WebDav,
    id: &str,
    owner: &Owner,
    seed: &DeviceSeed,
    new_password: Option<&str>,
) -> Result<DeviceFile> {
    paths::validate_installation_id(id)?;
    let _owner_lock = identity::OwnerLock::acquire(dav, id)?;
    let current = Owner::load_versioned(dav, id)?;
    if let Some((current, _)) = &current
        && (current.session_token != owner.session_token || current.pwd != owner.pwd)
    {
        bail!("设备身份或密码已变化，请重新读取后重试接管");
    }
    let mut device = DeviceFile {
        capabilities: seed.capabilities,
        capabilities_key: seed.capabilities_key.clone(),
        installation_id: id.to_owned(),
        sync_dir: seed.sync_dir.clone(),
        machine_fp: seed.machine_fp.clone(),
        host: seed.host.clone(),
        session_token: identity::new_session_token(),
        created_at: if owner.created_at.trim().is_empty() {
            seed.created_at.clone()
        } else {
            owner.created_at.clone()
        },
        last_sync: identity::now_stamp(),
        password: owner
            .pwd
            .as_ref()
            .map(PasswordHash::to_storage)
            .unwrap_or_default(),
    };
    if let Some(password) = new_password {
        device.password = PasswordHash::derive(password).to_storage();
    }
    let mut updated = owner.clone();
    updated.id = id.to_owned();
    updated.host = seed.host.clone();
    updated.app_ver = seed.app_ver.clone();
    updated.machine_fp = seed.machine_fp.clone();
    updated.session_token = device.session_token.clone();
    updated.last_sync = identity::now_stamp();
    if updated.created_at.trim().is_empty() {
        updated.created_at = seed.created_at.clone();
    }
    if new_password.is_some() {
        updated.pwd = device.password_hash();
        updated.pwd_set_at = Some(identity::now_stamp());
    }
    updated.taken_over_from = Some(Takeover {
        old_machine_fp: owner.machine_fp.clone(),
        at: identity::now_stamp(),
    });
    match current {
        Some((_, etag)) => {
            if let Some(etag) =
                etag.filter(|tag| seed.capabilities.if_match && !tag.starts_with("W/"))
            {
                updated.save_if_match(dav, id, &etag)?;
            } else {
                updated.save(dav, id)?;
            }
        }
        None => updated.save_if_absent(dav, id)?,
    }
    Ok(device)
}

/// 接管前把本地 `Others/` 备份到 `Rime/.takeover-backup-<ts>/`（放 `Sync/` 外，§7.2 第 2 步）。
pub fn backup_others(layout: &Layout) -> Result<Option<PathBuf>> {
    if !layout.others.is_dir() {
        return Ok(None);
    }
    let stamp = chrono::Local::now().format("%Y%m%d%H%M%S").to_string();
    let target = layout.rime.join(format!(".takeover-backup-{stamp}"));
    let mut copied = 0;
    for entry in walkdir::WalkDir::new(&layout.others) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(&layout.others)?;
        let destination = target.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(entry.path(), &destination)?;
        copied += 1;
    }
    if copied == 0 {
        let _ = fs::remove_dir_all(&target);
        return Ok(None);
    }
    Ok(Some(target))
}

/// §7.2 验证通过后的自动复原（保留远端快照 + 备份本地 Others + 覆盖 owner.json）。
pub fn takeover_verified(
    dav: &WebDav,
    id: &str,
    owner: &Owner,
    password: &str,
    seed: &DeviceSeed,
    layout: &Layout,
) -> Result<(DeviceFile, Vec<String>)> {
    verify(owner, password)?;
    takeover_common(dav, id, owner, seed, layout, None)
}

/// §7.3 强制接管：忘了密码或从未设密码时的兜底，**要求重设新密码**。
pub fn takeover_forced(
    dav: &WebDav,
    id: &str,
    owner: &Owner,
    new_password: &str,
    seed: &DeviceSeed,
    layout: &Layout,
) -> Result<(DeviceFile, Vec<String>)> {
    validate_remote_id(dav, id)?;
    identity::validate_password_input(new_password)?;
    takeover_common(dav, id, owner, seed, layout, Some(new_password))
}

fn takeover_common(
    dav: &WebDav,
    id: &str,
    owner: &Owner,
    seed: &DeviceSeed,
    layout: &Layout,
    new_password: Option<&str>,
) -> Result<(DeviceFile, Vec<String>)> {
    paths::validate_installation_id(id)?;
    let mut notices = Vec::new();
    // 第 1 步：远端 Sync/<ID>/ 的 userdb 快照原样保留（接管唯一的价值）。
    notices.push("已保留远端 Sync/<ID>/ 的 userdb 快照。".to_owned());
    // 第 2 步：本地 Others/ 先备份。
    if let Some(backup) = backup_others(layout)? {
        notices.push(format!("已把本地 Others/ 备份到 {}。", backup.display()));
    }
    // 先补齐快照；下载失败时保留原身份，重试不会丢失刚签发的 token。
    let restored = crate::transport::restore_own_snapshots(dav, layout)?;
    if restored > 0 {
        notices.push(format!("已恢复远端设备快照 {restored} 项。"));
    }
    // 第 3 步：覆盖 owner.json（新 machine_fp、新 session_token、接管留痕）。
    let device = rewrite_owner(dav, id, owner, seed, new_password)?;
    notices.push("已签发新的 session_token，旧设备下一轮会自动停止同步。".to_owned());
    Ok((device, notices))
}

/// §7.6 注销此设备：验证密码后删除远端 `<ID>/` 与本地 `Sync/<ID>/`。
pub fn retire(dav: &WebDav, id: &str, password: &str, layout: &Layout) -> Result<Vec<String>> {
    let _owner_lock = identity::OwnerLock::acquire(dav, id)?;
    let owner = Owner::load(dav, id)?.unwrap_or_default();
    // 从未设密码的 ID 允许直接注销；只有已设密码的 ID 需要验证。
    if owner.has_password() {
        verify(&owner, password)?;
    }
    dav.delete_collection(id)?;
    if layout.device.is_dir() {
        fs::remove_dir_all(&layout.device)?;
    }
    Ok(vec![format!(
        "已注销设备 {id}：远端 <ID>/ 与本地 Sync/{id}/ 均已删除。"
    )])
}

/// §7.7 修改 ID 密码：验证旧密码 → 重新派生 → 同时更新 owner.json 与本地派生串；**不轮换 token**。
pub fn change_password(
    dav: &WebDav,
    id: &str,
    old_password: &str,
    new_password: &str,
    device: &mut DeviceFile,
) -> Result<()> {
    paths::validate_installation_id(id)?;
    let _owner_lock = identity::OwnerLock::acquire(dav, id)?;
    identity::validate_password_input(new_password)?;
    let (mut owner, etag) = Owner::load_versioned(dav, id)?
        .with_context(|| format!("远端找不到 {id}/owner.json，无法修改密码"))?;
    if owner.has_password() {
        verify(&owner, old_password)?;
    }
    let derived = PasswordHash::derive(new_password);
    owner.pwd = Some(derived.clone());
    owner.pwd_set_at = Some(identity::now_stamp());
    if let Some(etag) = etag.filter(|tag| device.capabilities.if_match && !tag.starts_with("W/")) {
        owner.save_if_match(dav, id, &etag)?;
    } else {
        owner.save(dav, id)?;
    }
    device.password = derived.to_storage();
    Ok(())
}

/// §7.8 导出设备配置（不含密码）。
pub fn export(device: &DeviceFile, webdav_url: &str, list: &SyncList) -> identity::ExportedDevice {
    identity::ExportedDevice {
        id: device.installation_id.clone(),
        session_token: device.session_token.clone(),
        webdav_url: webdav_url.to_owned(),
        sync_files: list.files.iter().map(|file| file.path.clone()).collect(),
        excluded: list.excluded.clone(),
    }
}

/// §7.8 导入设备配置：ID 与 token 保持一致即可直接接着用，零冲突、无需接管。
pub fn import(
    exported: &identity::ExportedDevice,
    device: &mut DeviceFile,
    settings: &mut crate::config::Settings,
    layout: &Layout,
) -> Result<Vec<String>> {
    paths::validate_installation_id(&exported.id)?;
    let mut notices = Vec::new();
    device.installation_id = exported.id.clone();
    if !exported.session_token.trim().is_empty() {
        device.session_token = exported.session_token.clone();
    }
    if !exported.webdav_url.trim().is_empty() {
        settings.webdav_url = exported.webdav_url.clone();
    }
    let mut list = SyncList::default();
    for file in &exported.sync_files {
        list.register(&paths::validate_sync_file_path(file)?);
    }
    list.excluded = exported
        .excluded
        .iter()
        .filter(|item| paths::validate_sync_file_path(item).is_ok())
        .cloned()
        .collect();
    list.save(&layout.list_file())?;
    notices.push(format!(
        "已导入设备配置：ID = {}，同步列表 {} 项。导入不含密码，无需接管。",
        exported.id,
        list.files.len()
    ));
    Ok(notices)
}

/// 远端设备目录快照数量（接管确认弹窗展示用）。
pub fn count_remote_snapshots(dav: &WebDav, id: &str) -> usize {
    match dav.try_get_bytes(&format!("{id}/{SNAPSHOT_INDEX_FILE}")) {
        Ok(Some(bytes)) => serde_json::from_slice::<SnapshotIndex>(&bytes)
            .map(|index| index.files.len())
            .unwrap_or(0),
        _ => dav
            .list(id)
            .map(|entries| entries.iter().filter(|entry| !entry.is_dir).count())
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_reuses_existing_machine_fp() {
        let device = DeviceFile {
            machine_fp: "abc".into(),
            host: "pc".into(),
            created_at: "2026-01-01 00:00:00".into(),
            ..Default::default()
        };
        let seed = DeviceSeed::from_device(&device, "/tmp/Sync");
        assert_eq!(seed.machine_fp, "abc");
        assert_eq!(seed.host, "pc");
        assert_eq!(seed.sync_dir, "/tmp/Sync");
    }

    #[test]
    fn verify_rejects_ids_without_password() {
        let owner = Owner::default();
        assert!(verify(&owner, "anything").is_err());
    }

    #[test]
    fn forced_takeover_rejects_invalid_ids_before_any_network_or_file_action() {
        let dir = tempfile::tempdir().unwrap();
        let dav = WebDav::new("https://example.invalid/", "user", "password").unwrap();
        let seed = DeviceSeed::from_device(&DeviceFile::default(), "Sync");
        for id in ["../escape", "Others", "WebDAV", "CON"] {
            let layout = Layout::new(dir.path(), id);
            assert!(
                takeover_forced(&dav, id, &Owner::default(), "new-password", &seed, &layout)
                    .is_err()
            );
        }
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn import_rejects_private_paths_without_replacing_the_existing_sync_list() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path(), "pc01");
        layout.ensure_dirs().unwrap();
        let mut original = SyncList::default();
        original.register("custom_phrase.txt");
        original.save(&layout.list_file()).unwrap();
        let before = fs::read(layout.list_file()).unwrap();
        for path in [
            ".rime-sync/device.json",
            "words.userdb/CURRENT",
            "Sync/pc01/words.userdb.txt",
            "installation.yaml",
            "user.yaml",
        ] {
            let exported = identity::ExportedDevice {
                id: "pc01".into(),
                session_token: "token".into(),
                webdav_url: String::new(),
                sync_files: vec![path.into()],
                excluded: Vec::new(),
            };
            assert!(
                import(
                    &exported,
                    &mut DeviceFile::default(),
                    &mut crate::config::Settings::default(),
                    &layout
                )
                .is_err(),
                "imported {path}"
            );
            assert_eq!(fs::read(layout.list_file()).unwrap(), before);
        }
    }

    #[test]
    fn minutes_since_last_sync_parses_stamp() {
        let owner = Owner {
            last_sync: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            ..Default::default()
        };
        assert!(minutes_since_last_sync(&owner).unwrap() <= 1);
        let broken = Owner {
            last_sync: "not a date".into(),
            ..Default::default()
        };
        assert!(minutes_since_last_sync(&broken).is_none());
    }
}
