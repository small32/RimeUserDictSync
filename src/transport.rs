use crate::{
    context::Context,
    hash,
    identity::{self, Owner},
    paths::{
        OTHERS_DIR_NAME, OWNER_FILE, SNAPSHOT_INDEX_FILE, is_forbidden_in_others,
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

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, Default)]
pub struct RoundOutcome {
    pub downloaded: usize,
    pub uploaded: usize,
    pub notes: Vec<String>,
}

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

/// §7.5：每轮开始前校验 `session_token`，不匹配说明此 ID 已被其他设备接管。
pub fn check_session(context: &Context, report: &dyn Reporter) -> Result<()> {
    let Some(owner) = Owner::load(&context.dav, &context.layout.installation_id)? else {
        return Ok(());
    };
    if owner.session_token.trim().is_empty() {
        return Ok(());
    }
    if !context.device.has_session() {
        // 本机还没有令牌（例如刚导入配置前）：接受远端令牌，避免误判。
        report.log("提示：本地缺少 session_token，已按远端 owner.json 记录。");
        return Ok(());
    }
    if owner.session_token != context.device.session_token {
        bail!(
            "此 ID（{}）已被其他设备接管，本机已停止同步；请在「设备设置」中改用新的 ID 或重新导入设备配置。",
            context.layout.installation_id
        );
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
    let entries = context.dav.list_tree(OTHERS_DIR_NAME)?;
    let mut downloaded = 0;
    for entry in entries.iter().filter(|entry| !entry.is_dir) {
        let Some(relative) = strip_dir(&entry.relative, OTHERS_DIR_NAME) else {
            continue;
        };
        if relative.is_empty() {
            continue;
        }
        if is_forbidden_in_others(relative) {
            report.log(&format!(
                "跳过 Others/{relative}：userdb 快照不能出现在共享目录中。"
            ));
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
    let mut uploaded = 0;
    for (relative, path) in walk_files(&context.layout.others)? {
        check(cancel)?;
        if is_forbidden_in_others(&relative) {
            report.log(&format!(
                "跳过 Others/{relative}：userdb 快照不能放进共享目录（§2.2 红线）。"
            ));
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
    let sha256 = hash::sha256_file(local)?;
    match capabilities.tier() {
        UploadTier::Conditional => match dav.put(remote_relative, local, seen_etag)? {
            PutOutcome::PreconditionFailed => return Ok(UploadOutcome::PreconditionFailed),
            PutOutcome::Uploaded { .. } => {}
        },
        UploadTier::EtagOnly => {
            // 上传前重取一次 ETag，与本轮开始时读到的不同就不传（§4.4 B 档）。
            let current = dav.stat(remote_relative)?.and_then(|entry| entry.etag);
            if current.as_deref() != seen_etag {
                return Ok(UploadOutcome::PreconditionFailed);
            }
            match dav.put(remote_relative, local, None)? {
                PutOutcome::Uploaded { .. } => {}
                PutOutcome::PreconditionFailed => return Ok(UploadOutcome::PreconditionFailed),
            }
        }
        UploadTier::MetadataOnly => {
            if capabilities.move_supported {
                let temp = format!("{remote_relative}.tmp-{}", identity::new_path_token(8));
                match dav.put(&temp, local, None)? {
                    PutOutcome::Uploaded { .. } => {}
                    PutOutcome::PreconditionFailed => {
                        return Ok(UploadOutcome::PreconditionFailed);
                    }
                }
                if !dav.move_entry(&temp, remote_relative)? {
                    // 没有 MOVE 就退化为 PUT + 校验 hash（§10 风险 9）。
                    dav.put(remote_relative, local, None)?;
                    let _ = dav.delete(&temp);
                }
            } else {
                dav.put(remote_relative, local, None)?;
                let bytes = dav.get_bytes(remote_relative)?;
                if hash::sha256_hex(&bytes) != sha256 {
                    bail!("{remote_relative} 上传后校验失败（服务端内容与本地不一致）");
                }
                report.log(&format!("{remote_relative} 上传后已按 hash 校验通过。"));
            }
        }
    }
    // 统一用 PROPFIND 读回的 ETag 作为下次粗筛依据：PUT 响应里的 ETag 可能与
    // PROPFIND 的写法不同（是否带引号、强弱标记），混用会导致反复重传。
    let etag = dav.stat(remote_relative)?.and_then(|entry| entry.etag);
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
        for (file, snapshot) in &remote_index.files {
            crate::state::safe_snapshot_name(file)?;
            let target = target_dir.join(file);
            let unchanged = marks.get(file).map(String::as_str) == Some(snapshot.sha256.as_str())
                && target.is_file();
            if unchanged {
                continue;
            }
            let remote_relative = format!("{name}/{file}");
            // 远端清单里已经有 mtime，直接用它可以省掉每个文件一次 PROPFIND。
            let modified = (snapshot.mtime > 0).then(|| {
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(snapshot.mtime as u64)
            });
            dav.get(&remote_relative, &target, modified)?;
            marks.insert(file.clone(), snapshot.sha256.clone());
            report.log(&format!("已下载其他设备快照 {name}/{file}。"));
            downloaded += 1;
        }
        // 远端清单里已删除的快照：本地镜像也要删掉，否则 librime 会一直合并旧快照。
        let removed: Vec<String> = marks
            .keys()
            .filter(|file| !remote_index.files.contains_key(*file))
            .cloned()
            .collect();
        for file in removed {
            let target = target_dir.join(&file);
            if target.is_file() {
                fs::remove_file(&target)?;
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
            !seen_devices
                .iter()
                .any(|seen| seen.eq_ignore_ascii_case(name))
        })
        .cloned()
        .collect();
    for name in retired {
        index.snapshots.remove(&name);
        let dir = context.layout.sync.join(&name);
        if dir.is_dir() {
            fs::remove_dir_all(&dir)?;
            report.log(&format!("远端设备 {name}/ 已不存在，本地镜像目录已清理。"));
        }
    }
    Ok(downloaded)
}

// --------------------------------------------------------------- 本机快照

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
    let remote_index: SnapshotIndex = match context.dav.try_get_bytes(&remote_relative_index)? {
        Some(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        None => SnapshotIndex::default(),
    };
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
        let unchanged = snapshot_index
            .files
            .get(&name)
            .is_some_and(|entry| entry.sha256 == sha256)
            && remote_index
                .files
                .get(&name)
                .is_some_and(|entry| entry.sha256 == sha256);
        let remote_relative = format!("{}/{name}", context.layout.installation_id);
        if unchanged {
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
            UploadOutcome::Uploaded { .. } => {
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
                report.log(&format!(
                    "{remote_relative} 上传前 ETag 变化，本轮跳过（下轮重试）。"
                ));
            }
        }
    }
    snapshot_index
        .files
        .retain(|name, _| present.iter().any(|item| item == name));
    snapshot_index.generated_at = identity::now_stamp();
    snapshot_index.save(&path)?;
    // 清单本身最后上传，保证别人读到的清单一定有对应的文件。
    context.dav.put(&remote_relative_index, &path, None)?;

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

/// 更新远端 `owner.json` 的 `last_sync` 等字段（§7.1）。
fn touch_owner(context: &Context, report: &dyn Reporter) -> Result<()> {
    let id = &context.layout.installation_id;
    let mut owner = Owner::load(&context.dav, id)?.unwrap_or_default();
    if owner.id.is_empty() {
        owner.id = id.clone();
    }
    owner.host = context.device.host.clone();
    owner.app_ver = APP_VERSION.to_owned();
    owner.machine_fp = context.device.machine_fp.clone();
    if owner.created_at.trim().is_empty() {
        owner.created_at = context.device.created_at.clone();
    }
    if owner.session_token.trim().is_empty() && context.device.has_session() {
        owner.session_token = context.device.session_token.clone();
    }
    if owner.pwd.is_none() {
        owner.pwd = context.device.password_hash();
    }
    owner.last_sync = identity::now_stamp();
    owner.save(&context.dav, id)?;
    report.log(&format!(
        "已更新远端 owner.json（last_sync: {}）。",
        owner.last_sync
    ));
    Ok(())
}

// ------------------------------------------------------------------- 工具

fn strip_dir<'a>(relative: &'a str, dir: &str) -> Option<&'a str> {
    let prefix = format!("{dir}/");
    relative.strip_prefix(&prefix)
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

    #[test]
    fn strip_dir_removes_only_the_requested_prefix() {
        assert_eq!(strip_dir("Others/a.txt", "Others"), Some("a.txt"));
        assert_eq!(strip_dir("Others/cn/b.yaml", "Others"), Some("cn/b.yaml"));
        assert_eq!(strip_dir("Other/a.txt", "Others"), None);
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
}
