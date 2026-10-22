use crate::{
    context::Context,
    hash,
    identity::{self, Owner},
    paths::{
        Layout, OTHERS_DIR_NAME, OWNER_FILE, SNAPSHOT_INDEX_FILE, is_forbidden_in_others,
        validate_installation_id,
    },
    report::Reporter,
    state::{IndexEntry, LocalLock, RemoteMark, SnapshotEntry, SnapshotIndex, SyncIndex},
    webdav::{PutOutcome, RemoteEntry, UploadTier},
};
use anyhow::{Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use walkdir::WalkDir;

#[derive(Clone, Debug, Default)]
pub struct RoundOutcome {
    pub downloaded: usize,
    pub uploaded: usize,
    pub notes: Vec<String>,
}

/// 本设备的 ID 已被其他设备接管。
///
/// 这不是网络故障：继续同步会往别人的 `<ID>/` 写快照、覆盖对方的 `.index.json`。
/// 调用方（尤其手动同步）必须据此直接中止，而不是「忽略错误继续跑」。
#[derive(Clone, Debug)]
pub struct SessionTakenOver {
    pub installation_id: String,
}

impl std::fmt::Display for SessionTakenOver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "此 ID（{}）已被其他设备接管，本机已停止同步；请在「设备设置」中改用新的 ID 或重新导入设备配置。",
            self.installation_id
        )
    }
}

impl std::error::Error for SessionTakenOver {}

/// 后台搬运一轮（取锁）。**只搬运文件，不读写 `RIME/` 根目录下的源文件**（§3）。
pub fn round(
    context: &Context,
    cancel: &AtomicBool,
    report: &dyn Reporter,
) -> Result<RoundOutcome> {
    let _lock = LocalLock::acquire(&context.layout.lock_file())?;
    round_locked(context, cancel, report)
}

/// 已经持有本地锁时执行一轮搬运（手动同步的第 ① 步会调用它）。
pub fn round_locked(
    context: &Context,
    cancel: &AtomicBool,
    report: &dyn Reporter,
) -> Result<RoundOutcome> {
    let mut outcome = RoundOutcome::default();
    check_local_identity(context)?;
    report.progress(5);

    check_session(context, report)?;
    check(cancel)?;

    // ① 拉：远端 Others/ → 本地 Sync/Others/
    report.log("后台搬运：比对远端 Others/ 与本地共享目录。");
    let mut index = context.load_index()?;
    let pulled = pull_others(context, &mut index, report)?;
    outcome.downloaded += pulled;
    context.save_index(&index)?;
    report.progress(35);
    check(cancel)?;

    // ② 拉：其他设备 <ID_x>/.index.json → 增量下载 userdb 快照
    report.log("后台搬运：比对其他设备的 userdb 快照清单。");
    let snapshots = pull_snapshots(context, &mut index, report)?;
    outcome.downloaded += snapshots;
    context.save_index(&index)?;
    report.progress(60);
    check(cancel)?;

    // ③ 推：本地 Others/ 的变化
    report.log("后台搬运：上传本地 Others/ 的变化。");
    let uploaded = push_others(context, &mut index, cancel, report)?;
    outcome.uploaded += uploaded;
    context.save_index(&index)?;
    report.progress(85);
    check(cancel)?;

    // ④ 推：本设备 <ID_A>/ 快照 + `.index.json`（§11 P0：本机快照必须上得去）
    report.log("后台搬运：上传本设备的 userdb 快照。");
    let pushed = push_own_snapshot(context, cancel, report)?;
    outcome.uploaded += pushed;
    touch_owner(context, report)?;
    report.progress(100);
    outcome.notes.push(format!(
        "后台搬运完成：下载 {} 项，上传 {} 项。",
        outcome.downloaded, outcome.uploaded
    ));
    Ok(outcome)
}

/// Context 构建与取得同步锁之间，设备设置可能已切换；不能使用旧身份写同一份私有索引。
pub fn check_local_identity(context: &Context) -> Result<()> {
    let path = context.layout.device_file();
    if path.is_file() {
        let current = crate::context::DeviceFile::load(&path)?;
        if current.installation_id != context.device.installation_id
            || current.session_token != context.device.session_token
        {
            bail!("设备设置已发生变化，请重新加载后再同步");
        }
    }
    Ok(())
}

/// §7.5：每轮开始前校验 `session_token`，不匹配说明此 ID 已被其他设备接管。
pub fn check_session(context: &Context, _report: &dyn Reporter) -> Result<()> {
    let Some(owner) = Owner::load(&context.dav, &context.layout.installation_id)? else {
        bail!(
            "远端 owner.json 缺失或损坏，无法校验设备身份；本轮搬运已中止（不下载、不上传），请检查设备设置"
        );
    };
    if owner.session_token.trim().is_empty() {
        bail!("远端 owner.json 缺少 session_token，无法校验设备身份，请重新完成设备设置");
    }
    if !context.device.has_session() {
        bail!("本地缺少 session_token，请重新完成设备设置");
    }
    if owner.session_token != context.device.session_token {
        return Err(anyhow::Error::new(SessionTakenOver {
            installation_id: context.layout.installation_id.clone(),
        }));
    }
    Ok(())
}

fn check(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("用户已停止同步");
    }
    Ok(())
}

// ---------------------------------------------------------------- Others 拉取

fn pull_others(context: &Context, index: &mut SyncIndex, report: &dyn Reporter) -> Result<usize> {
    let list = context.load_list()?;
    let entries = context.dav.list_tree(OTHERS_DIR_NAME)?;
    let mut downloaded = 0;
    for entry in entries.iter().filter(|entry| !entry.is_dir) {
        let Some(relative) = strip_dir(&entry.relative, OTHERS_DIR_NAME) else {
            continue;
        };
        if relative.is_empty() || is_transfer_temp(relative) {
            continue;
        }
        if list.is_excluded(relative) {
            continue;
        }
        if is_forbidden_in_others(relative) {
            report.log(&format!(
                "跳过 Others/{relative}：userdb 快照不能出现在共享目录中。"
            ));
            continue;
        }
        if crate::paths::is_private_in_others(relative) {
            continue;
        }
        let target = context.layout.others_path(relative);
        let mark = index.remote.get(relative).cloned().unwrap_or_default();
        if mark_matches_entry(&mark, entry) && target.is_file() {
            continue;
        }
        context.dav.get(&entry.relative, &target, entry.modified)?;
        let local_hash = hash::sha256_file(&target)?;
        if mark.sha256.as_deref() == Some(local_hash.as_str()) {
            // 只有元数据变化，内容其实一样。
            report.log(&format!("Others/{relative} 仅元数据变化，内容未变。"));
        } else {
            report.log(&format!("已下载 Others/{relative}。"));
            downloaded += 1;
        }
        index.remote.insert(
            relative.to_owned(),
            RemoteMark {
                etag: entry.etag.clone(),
                size: entry.length,
                mtime: entry.modified_secs(),
                sha256: Some(local_hash),
            },
        );
    }
    Ok(downloaded)
}

/// 用 PROPFIND 的 etag / size+mtime 粗筛，拿不到元数据时才需要下载全文（§3、§4.4 C 档）。
fn mark_matches_entry(mark: &RemoteMark, entry: &RemoteEntry) -> bool {
    if let (Some(stored), Some(current)) = (mark.etag.as_deref(), entry.etag.as_deref()) {
        return stored == current;
    }
    if let (Some(size), Some(mtime)) = (mark.size, mark.mtime) {
        return Some(size) == entry.length && Some(mtime) == entry.modified_secs();
    }
    false
}

// ------------------------------------------------------------ Others 上传

/// 上传本地 `Sync/Others/` 的变更；返回上传数量。
pub fn push_others(
    context: &Context,
    index: &mut SyncIndex,
    cancel: &AtomicBool,
    report: &dyn Reporter,
) -> Result<usize> {
    let list = context.load_list()?;
    let mut uploaded = 0;
    for (relative, path) in walk_files(&context.layout.others)? {
        check(cancel)?;
        if list.is_excluded(&relative) {
            continue;
        }
        if is_forbidden_in_others(&relative) {
            report.log(&format!(
                "跳过 Others/{relative}：userdb 快照不能放进共享目录（§2.2 红线）。"
            ));
            continue;
        }
        if crate::paths::is_private_in_others(&relative) {
            continue;
        }
        if !relative.is_ascii() {
            report.log(&format!(
                "警告：Others/{relative} 不是纯 ASCII 文件名，跨平台可能出现重名。"
            ));
        }
        let local_hash = hash::sha256_file(&path)?;
        let mark = index.remote.get(&relative).cloned().unwrap_or_default();
        if mark.sha256.as_deref() == Some(local_hash.as_str()) {
            continue;
        }
        let remote_relative = format!("{OTHERS_DIR_NAME}/{relative}");
        match upload(
            context,
            &remote_relative,
            &path,
            mark.etag.as_deref(),
            report,
        )? {
            UploadOutcome::Uploaded { etag, sha256 } => {
                // 只有远端已确认的本地源文件才能成为三向判断基线。
                // 上传失败/412 时保留原基线，下一轮才能识别双端修改。
                if let Some(source) = index.pending_files.get(&relative).cloned()
                    && source.sha256 == sha256
                {
                    index.files.insert(relative.clone(), source);
                    index.pending_files.remove(&relative);
                    index.conflicts.remove(&relative);
                }
                index.remote.insert(
                    relative.clone(),
                    RemoteMark {
                        etag,
                        size: Some(hash::file_size(&path)),
                        mtime: Some(hash::mtime_secs(&path)),
                        sha256: Some(sha256),
                    },
                );
                report.log(&format!("已上传 Others/{relative}。"));
                uploaded += 1;
            }
            UploadOutcome::PreconditionFailed => {
                report.log(&format!(
                    "Others/{relative} 在本次同步期间已被其他设备更新，本轮不再上传，留到下一轮处理。"
                ));
            }
        }
    }
    Ok(uploaded)
}

pub enum UploadOutcome {
    Uploaded {
        etag: Option<String>,
        sha256: String,
    },
    PreconditionFailed,
}

/// 按服务端能力选择上传校验档位，并按需做原子发布与上传后校验（§4.4、§9）。
fn upload(
    context: &Context,
    remote_relative: &str,
    local: &Path,
    seen_etag: Option<&str>,
    report: &dyn Reporter,
) -> Result<UploadOutcome> {
    let dav = &context.dav;
    let capabilities = context.device.capabilities;
    // hash 与请求体使用同一次读取，避免 RIME 同时生成快照时清单与上传内容错配。
    let bytes = fs::read(local)?;
    let sha256 = hash::sha256_hex(&bytes);
    let etag = match capabilities.tier() {
        UploadTier::Conditional => {
            let result = match seen_etag {
                Some(etag) => dav.put_bytes(remote_relative, bytes, Some(etag))?,
                None => dav.put_bytes_if_absent(remote_relative, bytes)?,
            };
            match result {
                PutOutcome::PreconditionFailed => return Ok(UploadOutcome::PreconditionFailed),
                PutOutcome::Uploaded { etag } => etag,
            }
        }
        UploadTier::EtagOnly => {
            // 上传前重取一次 ETag，与本轮开始时读到的不同就不传（§4.4 B 档）。
            let current = dav.stat(remote_relative)?.and_then(|entry| entry.etag);
            if current.as_deref() != seen_etag {
                return Ok(UploadOutcome::PreconditionFailed);
            }
            match dav.put_bytes(remote_relative, bytes, None)? {
                PutOutcome::Uploaded { etag } => etag,
                PutOutcome::PreconditionFailed => return Ok(UploadOutcome::PreconditionFailed),
            }
        }
        UploadTier::MetadataOnly => {
            if capabilities.move_supported {
                let temp = format!("{remote_relative}.tmp-{}", identity::new_path_token(8));
                let result = (|| -> Result<PutOutcome> {
                    if matches!(
                        dav.put_bytes(&temp, bytes.clone(), None)?,
                        PutOutcome::PreconditionFailed
                    ) {
                        return Ok(PutOutcome::PreconditionFailed);
                    }
                    if !dav.move_entry(&temp, remote_relative)? {
                        return dav.put_bytes(remote_relative, bytes, None);
                    }
                    Ok(PutOutcome::Uploaded { etag: None })
                })();
                // MOVE、PUT 或响应失败时同样清理，避免每次重试留下一份完整词库。
                let _ = dav.delete(&temp);
                if matches!(result?, PutOutcome::PreconditionFailed) {
                    return Ok(UploadOutcome::PreconditionFailed);
                }
            } else {
                if matches!(
                    dav.put_bytes(remote_relative, bytes, None)?,
                    PutOutcome::PreconditionFailed
                ) {
                    return Ok(UploadOutcome::PreconditionFailed);
                }
            }
            let Some((remote_bytes, etag)) = dav.try_get_bytes_with_etag(remote_relative)? else {
                bail!("{remote_relative} 上传后无法读取");
            };
            if hash::sha256_hex(&remote_bytes) != sha256 {
                bail!("{remote_relative} 上传后校验失败（服务端内容与本地不一致）");
            }
            report.log(&format!("{remote_relative} 上传后已按 hash 校验通过。"));
            etag
        }
    };
    // 只能记录与本次 PUT（或校验 GET）同一响应的 ETag；独立 STAT 可能已读到他机版本。
    Ok(UploadOutcome::Uploaded { etag, sha256 })
}

// ---------------------------------------------------------- 其他设备的快照

/// 增量拉取其他设备的 `<ID>/.index.json` 清单与 userdb 快照（§3 步骤 ②）。
fn pull_snapshots(
    context: &Context,
    index: &mut SyncIndex,
    report: &dyn Reporter,
) -> Result<usize> {
    let dav = &context.dav;
    let own = context.layout.installation_id.clone();
    let mut downloaded = 0;
    let mut seen_devices: Vec<String> = Vec::new();

    for entry in dav.list("")? {
        if !entry.is_dir {
            continue;
        }
        let name = entry.relative.trim_end_matches('/').to_owned();
        if name.is_empty()
            || name.eq_ignore_ascii_case(OTHERS_DIR_NAME)
            || name.eq_ignore_ascii_case(&own)
            || name.eq_ignore_ascii_case("WebDAV")
        {
            continue;
        }
        if validate_installation_id(&name).is_err() {
            report.log(&format!(
                "远端目录 {name} 不是合法的设备 ID，已跳过（防止越界路径）。"
            ));
            continue;
        }
        seen_devices.push(name.clone());
        let Some(bytes) = dav.try_get_bytes(&format!("{name}/{SNAPSHOT_INDEX_FILE}"))? else {
            report.log(&format!(
                "远端设备目录 {name}/ 没有 .index.json，本轮跳过（无法判断增量）。"
            ));
            continue;
        };
        // 清单解析失败**不能**当成空清单：那会顺着「远端已删除」逻辑删光该设备本地镜像。
        let remote_index = match serde_json::from_slice::<SnapshotIndex>(&bytes) {
            Ok(index) => index,
            Err(error) => {
                report.log(&format!(
                    "远端设备目录 {name}/ 的 .index.json 无法解析（{error}），本轮跳过。"
                ));
                continue;
            }
        };
        let marks = index.snapshots.entry(name.clone()).or_default();
        let target_dir = context.layout.sync.join(&name);
        // 旧版曾搬运配置文件：只移动已跟踪的镜像，保留副本供用户检查。
        let legacy: Vec<String> = marks
            .keys()
            .filter(|file| !file.to_ascii_lowercase().ends_with(".userdb.txt"))
            .cloned()
            .collect();
        for file in legacy {
            if crate::state::safe_snapshot_name(&file).is_err() {
                marks.remove(&file);
                continue;
            }
            let target = target_dir.join(&file);
            let result = (|| -> Result<()> {
                if target.is_file() {
                    let backup_dir = context.layout.meta.join("quarantine/snapshots").join(&name);
                    fs::create_dir_all(&backup_dir)?;
                    let backup = backup_dir.join(&file);
                    let backup = if backup.exists() {
                        hash::temp_sibling(&backup)
                    } else {
                        backup
                    };
                    fs::rename(&target, &backup)?;
                    report.log(&format!(
                        "旧版非 userdb 镜像 {name}/{file} 已移至 {}。",
                        backup.display()
                    ));
                }
                Ok(())
            })();
            match result {
                Ok(()) => {
                    marks.remove(&file);
                }
                Err(error) => report.log(&format!(
                    "清理旧版非 userdb 镜像 {name}/{file} 失败：{error}；稍后重试。"
                )),
            }
        }
        for (file, snapshot) in &remote_index.files {
            if let Err(error) = crate::state::safe_snapshot_name(file) {
                marks.remove(file);
                report.log(&format!("跳过远端快照 {name}/{file}：{error}"));
                continue;
            }
            if !file.to_ascii_lowercase().ends_with(".userdb.txt") {
                report.log(&format!(
                    "跳过远端 {name}/{file}：快照清单只搬运 .userdb.txt 文件。"
                ));
                continue;
            }
            let target = target_dir.join(file);
            let unchanged = marks.get(file).map(String::as_str) == Some(snapshot.sha256.as_str())
                && target.is_file();
            if unchanged {
                continue;
            }
            let remote_relative = format!("{name}/{file}");
            if let Err(error) = download_snapshot(dav, &remote_relative, &target, snapshot) {
                report.log(&format!(
                    "下载快照 {name}/{file} 失败：{error}；保留现有副本，稍后重试。"
                ));
                continue;
            }
            marks.insert(file.clone(), snapshot.sha256.clone());
            report.log(&format!("已下载其他设备快照 {name}/{file}。"));
            downloaded += 1;
        }
        // 远端清单里已删除的快照：本地镜像也要删掉，否则 librime 会一直合并旧快照。
        let removed: Vec<String> = marks
            .keys()
            .filter(|file| {
                file.to_ascii_lowercase().ends_with(".userdb.txt")
                    && !remote_index.files.contains_key(*file)
            })
            .cloned()
            .collect();
        for file in removed {
            // 旧版本可能已把非法名称写进私有索引，升级后的清理也必须校验。
            if crate::state::safe_snapshot_name(&file).is_err() {
                marks.remove(&file);
                continue;
            }
            let target = target_dir.join(&file);
            if target.is_file() {
                if let Err(error) = fs::remove_file(&target) {
                    report.log(&format!(
                        "删除已撤下的镜像 {name}/{file} 失败：{error}；稍后重试。"
                    ));
                    continue;
                }
                report.log(&format!("远端已删除 {name}/{file}，本地镜像同步删除。"));
            }
            marks.remove(&file);
        }
    }

    // 远端已经注销的设备目录：清理本地镜像（§7.6）。
    let retired: Vec<String> = index
        .snapshots
        .keys()
        .filter(|name| {
            !name.eq_ignore_ascii_case(&own)
                && !seen_devices
                    .iter()
                    .any(|seen| seen.eq_ignore_ascii_case(name))
        })
        .cloned()
        .collect();
    for name in retired {
        index.snapshots.remove(&name);
        if validate_installation_id(&name).is_err() {
            continue;
        }
        let dir = context.layout.sync.join(&name);
        if dir.is_dir() {
            fs::remove_dir_all(&dir)?;
            report.log(&format!("远端设备 {name}/ 已不存在，本地镜像目录已清理。"));
        }
    }
    Ok(downloaded)
}

// --------------------------------------------------------------- 本机快照

fn restored_snapshot_dir(layout: &Layout) -> PathBuf {
    // 该目录名不可能被注册为设备 ID，且独立于本 ID，避免 /sync 导出时覆盖待恢复快照。
    layout
        .sync
        .join(format!("__rime_restore_{}", layout.installation_id))
}

pub fn clear_restored_snapshots(layout: &Layout) -> Result<()> {
    let dir = restored_snapshot_dir(layout);
    if dir.is_dir() {
        fs::remove_dir_all(dir)?;
    }
    Ok(())
}

/// 接管/导入后保留两台设备的快照；已有本机快照不覆盖，远端版本供下一次 /sync 合并。
pub fn restore_own_snapshots(dav: &crate::webdav::WebDav, layout: &Layout) -> Result<usize> {
    let Some(bytes) = dav.try_get_bytes(&format!(
        "{}/{}",
        layout.installation_id, SNAPSHOT_INDEX_FILE
    ))?
    else {
        return Ok(0);
    };
    let remote_index: SnapshotIndex = serde_json::from_slice(&bytes)?;
    restore_own_from_index(dav, layout, &remote_index, false)
}

fn restore_own_from_index(
    dav: &crate::webdav::WebDav,
    layout: &Layout,
    remote_index: &SnapshotIndex,
    only_missing: bool,
) -> Result<usize> {
    let mut restored = 0;
    for (name, entry) in &remote_index.files {
        crate::state::safe_snapshot_name(name)?;
        if !name.to_ascii_lowercase().ends_with(".userdb.txt") {
            continue;
        }
        let own = layout.device.join(name);
        if only_missing && own.is_file() {
            continue;
        }
        let peer = restored_snapshot_dir(layout).join(name);
        download_snapshot(
            dav,
            &format!("{}/{name}", layout.installation_id),
            &peer,
            entry,
        )?;
        if !own.is_file() {
            fs::create_dir_all(&layout.device)?;
            fs::copy(&peer, &own)?;
        }
        restored += 1;
    }
    Ok(restored)
}

fn download_snapshot(
    dav: &crate::webdav::WebDav,
    relative: &str,
    target: &Path,
    entry: &SnapshotEntry,
) -> Result<()> {
    let temp = hash::temp_sibling(target);
    let modified = (entry.mtime > 0)
        .then(|| std::time::UNIX_EPOCH + std::time::Duration::from_secs(entry.mtime as u64));
    let result = (|| -> Result<()> {
        dav.get(relative, &temp, modified)?;
        if hash::sha256_file(&temp)? != entry.sha256 {
            bail!("{relative} 与快照清单的 hash 不一致，未替换现有快照");
        }
        fs::rename(&temp, target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// 上传本设备 `Sync/<ID>/` 下的 userdb 快照与 `.index.json`（§3 步骤 ④、§11 P0）。
pub fn push_own_snapshot(
    context: &Context,
    cancel: &AtomicBool,
    report: &dyn Reporter,
) -> Result<usize> {
    let dir = &context.layout.device;
    fs::create_dir_all(dir)?;
    let path = context.layout.snapshot_index_file();
    let mut snapshot_index = SnapshotIndex::load(&path)?;
    snapshot_index.installation_id = context.layout.installation_id.clone();
    // 远端清单只花一次 GET，就能判断本机全部快照是否需要上传（§10 风险 10）。
    let remote_relative_index = format!("{}/{SNAPSHOT_INDEX_FILE}", context.layout.installation_id);
    let (remote_index, remote_index_etag): (SnapshotIndex, _) = match context
        .dav
        .try_get_bytes_with_etag(&remote_relative_index)?
    {
        Some((bytes, etag)) => (serde_json::from_slice(&bytes)?, etag),
        None => (SnapshotIndex::default(), None),
    };
    restore_own_from_index(&context.dav, &context.layout, &remote_index, true)?;
    let mut uploaded = 0;
    let mut skipped: Vec<String> = Vec::new();

    let mut present: Vec<String> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == OWNER_FILE || name == SNAPSHOT_INDEX_FILE || name.ends_with(".tmp") {
            continue;
        }
        // 快照目录里只该上传 userdb 快照（§1 L1、§8.2）。
        // librime 的 /sync 会把 user.yaml、各方案 yaml、*.dict.yaml 等一并复制进来，
        // 那些属于设备私有/方案自带，上传后会被别的设备当作快照拉走，可能反过来覆盖对方的配置。
        if !name.to_ascii_lowercase().ends_with(".userdb.txt") {
            skipped.push(name);
            continue;
        }
        present.push(name.clone());
        let file_path = entry.path();
        let sha256 = hash::sha256_file(&file_path)?;
        // 本地清单与远端清单都认为没变，且远端确实还有这条记录时才跳过。
        let unchanged = remote_index
            .files
            .get(&name)
            .is_some_and(|entry| entry.sha256 == sha256);
        let remote_relative = format!("{}/{name}", context.layout.installation_id);
        if unchanged {
            snapshot_index.files.insert(
                name.clone(),
                SnapshotEntry {
                    sha256,
                    size: hash::file_size(&file_path),
                    mtime: hash::mtime_secs(&file_path),
                },
            );
            continue;
        }
        check(cancel)?;
        // B 档要求「本轮开始时的 ETag」作基准；本机快照目录只有本机写，
        // 上传前先取一次即可，传 None 会被当成"远端不存在"而永远拒绝更新（§11 P0）。
        let seen_etag = context
            .dav
            .stat(&remote_relative)?
            .and_then(|entry| entry.etag);
        match upload(
            context,
            &remote_relative,
            &file_path,
            seen_etag.as_deref(),
            report,
        )? {
            UploadOutcome::Uploaded { sha256, .. } => {
                snapshot_index.files.insert(
                    name.clone(),
                    SnapshotEntry {
                        sha256,
                        size: hash::file_size(&file_path),
                        mtime: hash::mtime_secs(&file_path),
                    },
                );
                report.log(&format!("已上传本机快照 {remote_relative}。"));
                uploaded += 1;
            }
            UploadOutcome::PreconditionFailed => {
                bail!("{remote_relative} 上传前 ETag 变化，本轮保留远端清单，下轮重试");
            }
        }
    }
    snapshot_index
        .files
        .retain(|name, _| present.iter().any(|item| item == name));
    snapshot_index.generated_at = identity::now_stamp();
    snapshot_index.save(&path)?;
    // 清单本身最后上传，保证别人读到的清单一定有对应的文件。
    let manifest = fs::read(&path)?;
    let result = if context.device.capabilities.if_match {
        match remote_index_etag.as_deref() {
            Some(etag) => context
                .dav
                .put_bytes(&remote_relative_index, manifest, Some(etag))?,
            None => context
                .dav
                .put_bytes_if_absent(&remote_relative_index, manifest)?,
        }
    } else {
        context
            .dav
            .put_bytes(&remote_relative_index, manifest, None)?
    };
    if matches!(result, PutOutcome::PreconditionFailed) {
        bail!("本设备快照清单已被其他同步更新，本轮不再覆盖，下轮重新比对");
    }

    // 远端 <ID>/ 里若还留着非 userdb 的文件（旧版本把配置/方案文件也当快照传上去了），
    // 顺手清掉，否则别人会一直看到这些不该同步的东西。
    //
    // 只删非 `*.userdb.txt`：远端 <ID>/ 里的 userdb 快照可能是接管前那台机器的
    // 唯一一份（§7.2 第 1 步要求保留），绝不能被本地清单"没列到"就删掉。
    if let Ok(entries) = context.dav.list(&context.layout.installation_id) {
        for entry in entries.iter().filter(|entry| !entry.is_dir) {
            let Some(name) = entry.relative.rsplit('/').next() else {
                continue;
            };
            if name == OWNER_FILE
                || name == SNAPSHOT_INDEX_FILE
                || name.starts_with(".last-sync-")
                || name.to_ascii_lowercase().ends_with(".userdb.txt")
                || snapshot_index.files.contains_key(name)
            {
                continue;
            }
            let remote = format!("{}/{name}", context.layout.installation_id);
            match context.dav.delete(&remote) {
                Ok(()) => report.log(&format!("已清理远端不再同步的 {remote}。")),
                Err(error) => report.log(&format!("清理远端 {remote} 失败：{error}")),
            }
        }
    }

    // 只在"远端清单里还留着这些文件"时才提示，避免每轮刷屏；
    // 一旦它们从远端消失（旧版本上传过），这里就自然安静了。
    let stale: Vec<&String> = skipped
        .iter()
        .filter(|name| remote_index.files.contains_key(*name))
        .collect();
    if !stale.is_empty() {
        report.log(&format!(
            "快照目录中的 {} 个非 userdb 文件不再上传（配置与方案文件由各设备自管）：{}",
            stale.len(),
            stale
                .iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>()
                .join("、")
        ));
    }
    Ok(uploaded)
}

/// 心跳与 owner 分开写，旧设备的在途请求不能回滚密码或接管令牌。
fn touch_owner(context: &Context, report: &dyn Reporter) -> Result<()> {
    check_session(context, report)?;
    let id = &context.layout.installation_id;
    let stamp = identity::now_stamp();
    let relative = format!(
        "{id}/.last-sync-{}.json",
        hash::sha256_hex(context.device.session_token.as_bytes())
    );
    let bytes = serde_json::to_vec(&serde_json::json!({ "last_sync": stamp }))?;
    if matches!(
        context.dav.put_bytes(&relative, bytes, None)?,
        PutOutcome::PreconditionFailed
    ) {
        bail!("服务端拒绝更新同步心跳");
    }
    report.log(&format!("已更新设备同步心跳（last_sync: {stamp}）。"));
    Ok(())
}

// ------------------------------------------------------------------- 工具

fn strip_dir<'a>(relative: &'a str, dir: &str) -> Option<&'a str> {
    let prefix = format!("{dir}/");
    relative.strip_prefix(&prefix)
}

fn is_transfer_temp(relative: &str) -> bool {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    name.starts_with('.')
        || name.ends_with(".tmp")
        || name.rsplit_once(".tmp-").is_some_and(|(_, token)| {
            token.len() == 8 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

/// 列出目录下的全部文件，返回 `/` 分隔的相对路径。
pub fn walk_files(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return Ok(out);
    }
    for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        // 隐藏文件（.DS_Store）与下载残留（`.<name>.<ts>.tmp`）不属于同步内容：
        // 一次失败的下载会留下半截临时文件，传上去就变成所有人可见的"词库"。
        let file_name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if is_transfer_temp(&file_name) {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        out.push((relative, entry.path().to_path_buf()));
    }
    Ok(out)
}

/// 需要把 `Sync/Others/` 里的文件写到哪个本地路径；暴露给引擎与测试。
pub fn state_of(path: &Path) -> Result<Option<IndexEntry>> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(IndexEntry {
        sha256: hash::sha256_file(path)?,
        size: hash::file_size(path),
        mtime: hash::mtime_secs(path),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webdav::test_support::{Reply, Server, serve};
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::{Arc, Mutex},
    };

    #[derive(Default)]
    struct MemoryDav {
        files: BTreeMap<String, Vec<u8>>,
        fail_move: bool,
        reject_manifest: bool,
        concurrent_write: bool,
    }

    fn memory_server(state: Arc<Mutex<MemoryDav>>) -> Server {
        serve(move |request| {
            let mut state = state.lock().unwrap();
            let etag = state
                .files
                .get(&request.path)
                .map(|bytes| format!("\"{}\"", hash::sha256_hex(bytes)));
            match request.method.as_str() {
                "GET" => state
                    .files
                    .get(&request.path)
                    .cloned()
                    .map(Reply::versioned)
                    .unwrap_or_else(|| Reply::new(404, Vec::new())),
                "PUT" => {
                    if request
                        .headers
                        .get("if-none-match")
                        .is_some_and(|value| value == "*")
                        && etag.is_some()
                        || request
                            .headers
                            .get("if-match")
                            .is_some_and(|value| Some(value) != etag.as_ref())
                        || state.reject_manifest && request.path.ends_with("/.index.json")
                    {
                        return Reply::new(412, Vec::new());
                    }
                    state
                        .files
                        .insert(request.path.clone(), request.body.clone());
                    let mut reply = Reply::versioned(request.body.clone());
                    reply.status = 201;
                    if state.concurrent_write && request.path == "/Others/a.custom.yaml" {
                        state
                            .files
                            .insert(request.path.clone(), b"remote-v3".to_vec());
                    }
                    reply
                }
                "DELETE" => {
                    state.files.remove(&request.path);
                    Reply::new(204, Vec::new())
                }
                "MOVE" => {
                    if state.fail_move {
                        return Reply::new(500, Vec::new());
                    }
                    let destination = url::Url::parse(&request.headers["destination"]).unwrap();
                    if let Some(bytes) = state.files.remove(&request.path) {
                        state.files.insert(destination.path().to_owned(), bytes);
                    }
                    Reply::new(201, Vec::new())
                }
                "PROPFIND" => {
                    let mut xml = String::from("<d:multistatus xmlns:d=\"DAV:\">");
                    let mut dirs = BTreeSet::new();
                    for (path, bytes) in &state.files {
                        if request.path.ends_with('/') {
                            let Some(rest) = path.strip_prefix(&request.path) else {
                                continue;
                            };
                            if let Some((dir, _)) = rest.split_once('/') {
                                dirs.insert(format!("{}{dir}/", request.path));
                                continue;
                            }
                        } else if path != &request.path {
                            continue;
                        }
                        xml.push_str(&format!("<d:response><d:href>{path}</d:href><d:propstat><d:prop><d:getetag>\"{}\"</d:getetag><d:getcontentlength>{}</d:getcontentlength></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>", hash::sha256_hex(bytes), bytes.len()));
                    }
                    for dir in dirs {
                        xml.push_str(&format!("<d:response><d:href>{dir}</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"));
                    }
                    xml.push_str("</d:multistatus>");
                    Reply::new(207, xml.into_bytes())
                }
                _ => Reply::new(404, Vec::new()),
            }
        })
    }

    fn test_context(url: &str) -> (tempfile::TempDir, Context) {
        let dir = tempfile::tempdir().unwrap();
        let mut context = Context::for_test(dir.path(), "pc01");
        context.layout.ensure_dirs().unwrap();
        context.dav = crate::webdav::WebDav::new(url, "user", "pass").unwrap();
        (dir, context)
    }

    fn remote_snapshot(bytes: &[u8]) -> Arc<Mutex<MemoryDav>> {
        let index = SnapshotIndex {
            files: BTreeMap::from([(
                "words.userdb.txt".into(),
                SnapshotEntry {
                    sha256: hash::sha256_hex(bytes),
                    size: bytes.len() as u64,
                    mtime: 0,
                },
            )]),
            ..Default::default()
        };
        Arc::new(Mutex::new(MemoryDav {
            files: BTreeMap::from([
                ("/pc01/words.userdb.txt".into(), bytes.to_vec()),
                (
                    "/pc01/.index.json".into(),
                    serde_json::to_vec(&index).unwrap(),
                ),
            ]),
            ..Default::default()
        }))
    }

    #[test]
    fn strip_dir_removes_only_the_requested_prefix() {
        assert_eq!(strip_dir("Others/a.txt", "Others"), Some("a.txt"));
        assert_eq!(strip_dir("Others/cn/b.yaml", "Others"), Some("cn/b.yaml"));
        assert_eq!(strip_dir("Other/a.txt", "Others"), None);
    }

    #[test]
    fn excluded_shared_files_are_neither_downloaded_nor_uploaded() {
        let state = Arc::new(Mutex::new(MemoryDav {
            files: BTreeMap::from([("/Others/custom/phrases.txt".into(), b"remote".to_vec())]),
            ..Default::default()
        }));
        let server = memory_server(state.clone());
        let (_dir, context) = test_context(&server.url);
        let mut list = crate::state::SyncList::default();
        list.remove(r"custom\phrases.txt");
        context.save_list(&list).unwrap();
        let excluded = context.layout.others.join("custom/phrases.txt");
        fs::create_dir_all(excluded.parent().unwrap()).unwrap();
        fs::write(&excluded, b"local").unwrap();
        fs::write(context.layout.others.join("allowed.txt"), b"allowed").unwrap();
        let mut index = SyncIndex::default();
        pull_others(&context, &mut index, &crate::report::NullReporter).unwrap();
        assert_eq!(fs::read(&excluded).unwrap(), b"local");
        assert_eq!(
            push_others(
                &context,
                &mut index,
                &AtomicBool::new(false),
                &crate::report::NullReporter
            )
            .unwrap(),
            1
        );
        assert_eq!(
            state.lock().unwrap().files["/Others/custom/phrases.txt"],
            b"remote"
        );
        assert!(!server.requests.lock().unwrap().iter().any(|request| {
            matches!(request.method.as_str(), "GET" | "PUT")
                && request.path == "/Others/custom/phrases.txt"
        }));
    }

    #[test]
    fn bad_peer_snapshot_entries_do_not_block_valid_downloads_or_own_uploads() {
        let state = remote_snapshot(b"old own words");
        let snapshot = |bytes: &[u8]| SnapshotEntry {
            sha256: hash::sha256_hex(bytes),
            size: bytes.len() as u64,
            mtime: 0,
        };
        let peer_index = SnapshotIndex {
            files: BTreeMap::from([
                ("../bad.userdb.txt".into(), snapshot(b"bad")),
                ("broken.userdb.txt".into(), snapshot(b"expected")),
                ("missing.userdb.txt".into(), snapshot(b"missing")),
                ("good.userdb.txt".into(), snapshot(b"good words")),
                ("config.yaml".into(), snapshot(b"remote config")),
            ]),
            ..Default::default()
        };
        {
            let mut state = state.lock().unwrap();
            state.files.insert(
                "/pc01/owner.json".into(),
                serde_json::to_vec(&serde_json::json!({"id":"pc01", "session_token":"test-token"}))
                    .unwrap(),
            );
            state.files.insert(
                "/pc02/.index.json".into(),
                serde_json::to_vec(&peer_index).unwrap(),
            );
            state
                .files
                .insert("/pc02/good.userdb.txt".into(), b"good words".to_vec());
            state.files.insert(
                "/pc02/broken.userdb.txt".into(),
                b"mismatched data".to_vec(),
            );
            state
                .files
                .insert("/pc02/config.yaml".into(), b"remote config".to_vec());
            state.files.insert(
                "/pc03/.index.json".into(),
                serde_json::to_vec(&SnapshotIndex {
                    files: BTreeMap::from([("other.userdb.txt".into(), snapshot(b"other words"))]),
                    ..Default::default()
                })
                .unwrap(),
            );
            state
                .files
                .insert("/pc03/other.userdb.txt".into(), b"other words".to_vec());
        }
        let server = memory_server(state.clone());
        let (_dir, context) = test_context(&server.url);
        fs::write(
            context.layout.device.join("words.userdb.txt"),
            b"new own words",
        )
        .unwrap();
        let peer = context.layout.sync.join("pc02");
        fs::create_dir_all(&peer).unwrap();
        fs::write(peer.join("broken.userdb.txt"), b"preserved words").unwrap();
        fs::write(peer.join("config.yaml"), b"preserved config").unwrap();
        let mut index = SyncIndex::default();
        index.snapshots.insert(
            "pc02".into(),
            BTreeMap::from([
                ("broken.userdb.txt".into(), "previous hash".into()),
                ("config.yaml".into(), "previous config hash".into()),
            ]),
        );
        context.save_index(&index).unwrap();
        let outcome = round_locked(
            &context,
            &AtomicBool::new(false),
            &crate::report::NullReporter,
        )
        .unwrap();
        assert!(outcome.uploaded > 0);
        assert_eq!(
            fs::read(peer.join("broken.userdb.txt")).unwrap(),
            b"preserved words"
        );
        assert_eq!(
            fs::read(peer.join("good.userdb.txt")).unwrap(),
            b"good words"
        );
        assert_eq!(
            fs::read(context.layout.sync.join("pc03/other.userdb.txt")).unwrap(),
            b"other words"
        );
        assert_eq!(
            state.lock().unwrap().files["/pc01/words.userdb.txt"],
            b"new own words"
        );
        assert!(!peer.join("config.yaml").exists());
        assert_eq!(
            fs::read(
                context
                    .layout
                    .meta
                    .join("quarantine/snapshots/pc02/config.yaml")
            )
            .unwrap(),
            b"preserved config"
        );
        let saved = context.load_index().unwrap();
        assert!(!saved.snapshots["pc02"].contains_key("config.yaml"));
        assert_eq!(
            saved.snapshots["pc02"]["broken.userdb.txt"],
            "previous hash"
        );
        assert!(
            !server
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.method == "GET" && request.path == "/pc02/config.yaml")
        );
    }

    #[test]
    fn missing_owner_stops_all_transport_before_downloads() {
        let server = memory_server(Arc::new(Mutex::new(MemoryDav::default())));
        let (_dir, context) = test_context(&server.url);
        let error = round_locked(
            &context,
            &AtomicBool::new(false),
            &crate::report::NullReporter,
        )
        .unwrap_err();
        assert!(error.to_string().contains("本轮搬运已中止"));
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/pc01/owner.json");
    }

    #[test]
    fn walk_files_skips_hidden_and_leftover_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("custom_phrase.txt"), b"a\n").unwrap();
        std::fs::write(dir.path().join(".DS_Store"), b"x").unwrap();
        // 下载失败残留的临时文件：绝不能当成词库上传。
        std::fs::write(dir.path().join(".custom_phrase.txt.123456.tmp"), b"half").unwrap();
        let names: Vec<String> = walk_files(dir.path())
            .unwrap()
            .into_iter()
            .map(|(relative, _)| relative)
            .collect();
        assert_eq!(names, vec!["custom_phrase.txt".to_owned()]);
    }

    #[test]
    fn session_taken_over_stays_a_typed_error() {
        let error = anyhow::Error::new(SessionTakenOver {
            installation_id: "pc01".into(),
        });
        // 手动同步靠这个 downcast 判断“必须中止”，不能被当成普通网络错误吞掉。
        assert!(error.downcast_ref::<SessionTakenOver>().is_some());
        assert!(error.to_string().contains("pc01"));
    }

    #[test]
    fn etag_is_preferred_over_size_and_mtime() {
        let entry = RemoteEntry {
            relative: "Others/a.txt".into(),
            is_dir: false,
            etag: Some("\"v2\"".into()),
            length: Some(10),
            modified: None,
        };
        let stale = RemoteMark {
            etag: Some("\"v1\"".into()),
            size: Some(10),
            mtime: None,
            sha256: None,
        };
        assert!(!mark_matches_entry(&stale, &entry));
        let same = RemoteMark {
            etag: Some("\"v2\"".into()),
            ..Default::default()
        };
        assert!(mark_matches_entry(&same, &entry));
    }

    #[test]
    fn size_and_mtime_are_the_fallback_when_etag_is_unavailable() {
        let mut modified = std::time::SystemTime::UNIX_EPOCH;
        modified += std::time::Duration::from_secs(1_700_000_000);
        let entry = RemoteEntry {
            relative: "Others/a.txt".into(),
            is_dir: false,
            etag: None,
            length: Some(42),
            modified: Some(modified),
        };
        let mark = RemoteMark {
            etag: None,
            size: Some(42),
            mtime: Some(1_700_000_000),
            sha256: None,
        };
        assert!(mark_matches_entry(&mark, &entry));
        let changed = RemoteMark {
            size: Some(43),
            mtime: Some(1_700_000_000),
            ..Default::default()
        };
        assert!(!mark_matches_entry(&changed, &entry));
    }

    #[test]
    fn switching_identity_does_not_retire_its_existing_snapshot_dir() {
        let state = remote_snapshot(b"saved words");
        let server = memory_server(state);
        let (_dir, context) = test_context(&server.url);
        let snapshot = context.layout.device.join("words.userdb.txt");
        fs::write(&snapshot, b"saved words").unwrap();
        let mut index = SyncIndex::default();
        index.snapshots.insert("pc01".into(), BTreeMap::new());
        pull_snapshots(&context, &mut index, &crate::report::NullReporter).unwrap();
        assert!(snapshot.is_file());
    }

    #[test]
    fn cleanup_does_not_follow_unsafe_names_cached_by_older_versions() {
        let state = remote_snapshot(b"saved words");
        state.lock().unwrap().files.insert(
            "/pc02/.index.json".into(),
            serde_json::to_vec(&SnapshotIndex::default()).unwrap(),
        );
        let server = memory_server(state);
        let (_dir, context) = test_context(&server.url);
        fs::create_dir_all(context.layout.sync.join("pc02")).unwrap();
        let outside = context.layout.sync.join("outside.userdb.txt");
        fs::write(&outside, b"keep this file").unwrap();
        let unsafe_dir = context.layout.rime.join("keep-directory");
        fs::create_dir_all(&unsafe_dir).unwrap();
        let mut index = SyncIndex::default();
        index.snapshots.insert(
            "pc02".into(),
            BTreeMap::from([("../outside.userdb.txt".into(), "old".into())]),
        );
        index
            .snapshots
            .insert("../keep-directory".into(), BTreeMap::new());
        pull_snapshots(&context, &mut index, &crate::report::NullReporter).unwrap();
        assert!(outside.is_file());
        assert!(unsafe_dir.is_dir());
        assert!(!index.snapshots["pc02"].contains_key("../outside.userdb.txt"));
    }

    #[test]
    fn empty_local_snapshot_dir_restores_remote_manifest_and_merge_peer() {
        let state = remote_snapshot(b"old device words");
        let server = memory_server(state.clone());
        let (_dir, context) = test_context(&server.url);
        push_own_snapshot(
            &context,
            &AtomicBool::new(false),
            &crate::report::NullReporter,
        )
        .unwrap();
        assert_eq!(
            fs::read(context.layout.device.join("words.userdb.txt")).unwrap(),
            b"old device words"
        );
        assert_eq!(
            fs::read(restored_snapshot_dir(&context.layout).join("words.userdb.txt")).unwrap(),
            b"old device words"
        );
        let index: SnapshotIndex =
            serde_json::from_slice(&state.lock().unwrap().files["/pc01/.index.json"]).unwrap();
        assert!(index.files.contains_key("words.userdb.txt"));
    }

    #[test]
    fn takeover_restoration_keeps_both_local_and_remote_versions() {
        let state = remote_snapshot(b"remote words");
        let server = memory_server(state);
        let (_dir, context) = test_context(&server.url);
        let own = context.layout.device.join("words.userdb.txt");
        fs::write(&own, b"local words").unwrap();
        restore_own_snapshots(&context.dav, &context.layout).unwrap();
        assert_eq!(fs::read(&own).unwrap(), b"local words");
        assert_eq!(
            fs::read(restored_snapshot_dir(&context.layout).join("words.userdb.txt")).unwrap(),
            b"remote words"
        );
    }

    #[test]
    fn failed_manifest_publication_retries_without_losing_snapshot_entries() {
        let state = remote_snapshot(b"old words");
        state.lock().unwrap().reject_manifest = true;
        let server = memory_server(state.clone());
        let (_dir, mut context) = test_context(&server.url);
        context.device.capabilities.if_match = true;
        fs::write(context.layout.device.join("words.userdb.txt"), b"new words").unwrap();
        assert!(
            push_own_snapshot(
                &context,
                &AtomicBool::new(false),
                &crate::report::NullReporter
            )
            .is_err()
        );
        state.lock().unwrap().reject_manifest = false;
        push_own_snapshot(
            &context,
            &AtomicBool::new(false),
            &crate::report::NullReporter,
        )
        .unwrap();
        let state = state.lock().unwrap();
        let index: SnapshotIndex =
            serde_json::from_slice(&state.files["/pc01/.index.json"]).unwrap();
        assert_eq!(
            index.files["words.userdb.txt"].sha256,
            hash::sha256_hex(&state.files["/pc01/words.userdb.txt"])
        );
    }

    #[test]
    fn upload_keeps_its_response_etag_and_pulls_a_later_writer() {
        let state = Arc::new(Mutex::new(MemoryDav {
            files: BTreeMap::from([("/Others/a.custom.yaml".into(), b"base-v1".to_vec())]),
            concurrent_write: true,
            ..Default::default()
        }));
        let server = memory_server(state);
        let (_dir, mut context) = test_context(&server.url);
        context.device.capabilities.if_match = true;
        let other = context.layout.others.join("a.custom.yaml");
        fs::write(&other, b"local-v2").unwrap();
        let mut index = SyncIndex::default();
        index.remote.insert(
            "a.custom.yaml".into(),
            RemoteMark {
                etag: Some(format!("\"{}\"", hash::sha256_hex(b"base-v1"))),
                sha256: Some(hash::sha256_hex(b"base-v1")),
                ..Default::default()
            },
        );
        push_others(
            &context,
            &mut index,
            &AtomicBool::new(false),
            &crate::report::NullReporter,
        )
        .unwrap();
        assert_eq!(
            index.remote["a.custom.yaml"].etag,
            Some(format!("\"{}\"", hash::sha256_hex(b"local-v2")))
        );
        pull_others(&context, &mut index, &crate::report::NullReporter).unwrap();
        assert_eq!(fs::read(&other).unwrap(), b"remote-v3");
    }

    #[test]
    fn move_errors_clean_temporary_objects_and_leftovers_are_ignored() {
        let state = Arc::new(Mutex::new(MemoryDav {
            fail_move: true,
            ..Default::default()
        }));
        let server = memory_server(state.clone());
        let (_dir, mut context) = test_context(&server.url);
        context.device.capabilities.move_supported = true;
        fs::write(context.layout.others.join("words.dict.yaml"), b"dictionary").unwrap();
        let mut index = SyncIndex::default();
        assert!(
            push_others(
                &context,
                &mut index,
                &AtomicBool::new(false),
                &crate::report::NullReporter
            )
            .is_err()
        );
        assert!(state.lock().unwrap().files.is_empty());
        state.lock().unwrap().files.insert(
            "/Others/words.dict.yaml.tmp-1234abcd".into(),
            b"leftover".to_vec(),
        );
        pull_others(&context, &mut index, &crate::report::NullReporter).unwrap();
        assert!(
            !context
                .layout
                .others
                .join("words.dict.yaml.tmp-1234abcd")
                .exists()
        );
        fs::write(
            context.layout.others.join("local.txt.tmp-1234abcd"),
            b"leftover",
        )
        .unwrap();
        assert_eq!(walk_files(&context.layout.others).unwrap().len(), 1);
    }

    #[test]
    fn heartbeat_never_rewrites_password_or_session_owner() {
        let owner = serde_json::to_vec(
            &serde_json::json!({"id":"pc01","session_token":"test-token","host":"new host"}),
        )
        .unwrap();
        let state = Arc::new(Mutex::new(MemoryDav {
            files: BTreeMap::from([("/pc01/owner.json".into(), owner.clone())]),
            ..Default::default()
        }));
        let server = memory_server(state.clone());
        let (_dir, context) = test_context(&server.url);
        touch_owner(&context, &crate::report::NullReporter).unwrap();
        assert_eq!(state.lock().unwrap().files["/pc01/owner.json"], owner);
        assert!(
            server
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.method == "PUT")
                .all(|request| request.path.contains("/.last-sync-"))
        );
    }
}
