use crate::{
    context::Context,
    identity,
    merge::{MergeMode, merge_text_for_path, read_utf8},
    paths::Layout,
    platform,
    report::Reporter,
    state::{IndexEntry, LocalLock, SyncIndex, SyncList},
    transport,
};
use anyhow::{Context as _, Result, bail};
use std::{
    fs,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

/// 手动「同步词库」的结果。
#[derive(Clone, Debug, Default)]
pub struct ManualOutcome {
    pub merged: usize,
    pub uploaded: usize,
    pub conflicts: Vec<String>,
}

/// 每个登记文件本轮的处理结果。
#[derive(Clone, Debug, PartialEq, Eq)]
enum FileAction {
    Unchanged,
    Merged,
    PushedToOthers,
    PulledFromOthers,
    Conflict(String),
}

/// 手动「同步词库」（§4）。
///
/// 顺序不可颠倒：**拉 → 并集 → 写回 → 上传 → /sync → /deploy**（§4.2）。
///
/// 容错原则：**网络问题不阻止本机生效，本机生效失败才算失败。**
/// 所以所有与 WebDAV 有关的步骤都是尽力而为并把原因写进日志，
/// 而合并、写回、`/sync`、`/deploy` 仍然严格报错。
/// 这样离线时双击桌面「同步词库」也能把已经拉到本地的内容合并并生效。
pub fn manual_sync(
    context: &Context,
    cancel: &AtomicBool,
    report: &dyn Reporter,
) -> Result<ManualOutcome> {
    let _lock = LocalLock::acquire(&context.layout.lock_file())?;
    transport::check_local_identity(context)?;
    let mut outcome = ManualOutcome::default();
    report.progress(0);

    // 1. 先跑完一轮后台搬运，保证拿到远端最新内容。
    report.log("步骤 1/6：先执行一轮后台搬运（保证拉到远端最新内容）。");
    let mut uploads_allowed = match transport::round_locked(context, cancel, report) {
        Ok(round) => {
            for note in &round.notes {
                report.log(note);
            }
            true
        }
        Err(error) => {
            check(cancel)?;
            // 「ID 已被接管」不是网络问题：继续跑会往别人的 <ID>/ 写快照、
            // 覆盖对方的 .index.json，必须立刻中止。
            if error
                .downcast_ref::<crate::transport::SessionTakenOver>()
                .is_some()
            {
                return Err(error);
            }
            report.log(&format!(
                "步骤 1/6：拉取远端失败（{error}）；本轮只使用本地已有内容合并与生效，不再上传。"
            ));
            false
        }
    };
    report.progress(20);
    check(cancel)?;

    // 2. 只合并本设备 `.sync-list` 里登记的文件（§4.1）。
    let list = context.load_list()?;
    let mut index = context.load_index()?;
    report.log(&format!(
        "步骤 2/6：合并本设备登记的 {} 个同步文件。",
        list.files.len()
    ));
    for file in &list.files {
        check(cancel)?;
        match merge_registered(&context.layout, &mut index, &file.path, report) {
            Ok(FileAction::Unchanged) => {}
            Ok(FileAction::Conflict(copy)) => {
                outcome.conflicts.push(copy);
                outcome.merged += 1;
            }
            Ok(_) => outcome.merged += 1,
            Err(error) => {
                // 单个文件失败不应中断整轮手动同步（其余文件与 userdb 仍要生效）。
                report.log(&format!("合并 {} 失败：{error}", file.path));
            }
        }
    }
    context.save_index(&index)?;
    report.progress(45);
    check(cancel)?;

    // 3. 立即上传 Others/ 的变更，不等下一个后台轮次。
    uploads_allowed = uploads_allowed && session_allows_upload(context, report)?;
    if uploads_allowed {
        report.log("步骤 3/6：立即上传 Others/ 的变更。");
        match transport::push_others(context, &mut index, cancel, report) {
            Ok(uploaded) => outcome.uploaded = uploaded,
            Err(error) => {
                check(cancel)?;
                report.log(&format!(
                    "步骤 3/6：上传失败（{error}）；本地已合并的结果会在下一个后台轮次重试上传。"
                ));
            }
        }
        if let Err(error) = transport::push_own_snapshot(context, cancel, report) {
            check(cancel)?;
            report.log(&format!(
                "步骤 3/6：上传本机快照失败（{error}），稍后重试。"
            ));
        }
    }
    context.save_index(&index)?;
    report.progress(60);
    check(cancel)?;

    // 4. RIME /sync：合并 Sync/<各ID>/ 下的 userdb 快照（自造词与删除传播在此发生）。
    let rime = platform::find_rime(&context.settings.deployer_path, &context.base_dir)?;
    rime_sync(context, &rime, cancel, report, "步骤 4/6")?;
    report.progress(75);
    check(cancel)?;

    // 5. 把 /sync 产生的新快照推上去，手动同步本身也能让其他设备看到。
    uploads_allowed = uploads_allowed && session_allows_upload(context, report)?;
    if uploads_allowed {
        report.log("步骤 5/6：上传本次 /sync 产生的本机快照。");
        match transport::push_own_snapshot(context, cancel, report) {
            Ok(pushed) => outcome.uploaded += pushed,
            Err(error) => {
                check(cancel)?;
                report.log(&format!(
                    "步骤 5/6：上传本机快照失败（{error}），稍后重试。"
                ));
            }
        }
    }
    report.progress(90);
    check(cancel)?;

    // 6. /deploy --reload：让并集结果生效。
    rime_deploy(&rime, cancel, report, "步骤 6/6")
        .context("同步数据已保留，但 RIME 重新部署失败，请重新部署后生效")?;
    report.progress(100);
    report.log(&format!(
        "手动同步完成：处理 {} 个文件，上传 {} 项。",
        outcome.merged, outcome.uploaded
    ));
    Ok(outcome)
}

/// 只补齐后台服务做不到的两步：RIME 自带的用户资料同步与重新部署。
///
/// 后台服务的 `transport::round` 只搬运文件（拉取 → 上传），而 `/sync` 与 `/deploy`
/// 必须在**用户登录会话**里执行：它们要读该用户的配置，还要让正在运行的输入法重载，
/// session 0 里做不到。快捷方式因此只做这两步，与 macOS 菜单栏图标
/// （`Squirrel --sync` → `--reload`）保持同一策略。
///
/// 这里**不访问 WebDAV**，也**不把 `Sync/Others/` 写回 `RIME/`**：那些由后台服务
/// 与 GUI 的完整「同步词库」（[`manual_sync`]）负责。
pub fn rime_sync_and_deploy(
    context: &Context,
    cancel: &AtomicBool,
    report: &dyn Reporter,
) -> Result<()> {
    // 与服务共用同一把本地锁：`/sync` 要读 `Sync/<ID>/`，而服务可能正在往里写。
    let _lock = LocalLock::acquire(&context.layout.lock_file())?;
    transport::check_local_identity(context)?;
    let rime = platform::find_rime(&context.settings.deployer_path, &context.base_dir)?;
    report.progress(10);

    rime_sync(context, &rime, cancel, report, "步骤 1/2")?;
    report.progress(60);
    check(cancel)?;

    rime_deploy(&rime, cancel, report, "步骤 2/2")?;
    report.progress(100);
    report.log("已执行 RIME 用户资料同步并重新部署。");
    Ok(())
}

/// RIME 自带的用户资料同步：合并 `Sync/<各ID>/` 下的 userdb 快照。
///
/// 自造词合并与「删除」传播都发生在 librime 内部。跑完必须清掉它顺手复制进快照目录的
/// 非 userdb 资源，否则下一轮会被当成快照处理（§8.2：二进制模型不同步）。
fn rime_sync(
    context: &Context,
    rime: &platform::RimeCommand,
    cancel: &AtomicBool,
    report: &dyn Reporter,
    stage: &str,
) -> Result<()> {
    report.log(&format!(
        "{stage}：运行 RIME 用户资料同步（{} {}）。",
        rime.executable.display(),
        rime.sync_arg
    ));
    platform::run(rime, rime.sync_arg, cancel)?;
    transport::clear_restored_snapshots(&context.layout)?;
    let list = context.load_list()?;
    purge_snapshot_dir(context, &list, report)
}

/// 重新部署，让合并结果在输入法里生效。
fn rime_deploy(
    rime: &platform::RimeCommand,
    cancel: &AtomicBool,
    report: &dyn Reporter,
    stage: &str,
) -> Result<()> {
    report.log(&format!(
        "{stage}：重新部署 RIME 使词库生效（{} {}）。",
        rime.executable.display(),
        rime.deploy_arg
    ));
    platform::run(rime, rime.deploy_arg, cancel)
}

fn check(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("用户已停止同步");
    }
    Ok(())
}

fn session_allows_upload(context: &Context, report: &dyn Reporter) -> Result<bool> {
    match transport::check_session(context, report) {
        Ok(()) => Ok(true),
        Err(error)
            if error
                .downcast_ref::<transport::SessionTakenOver>()
                .is_some() =>
        {
            Err(error)
        }
        Err(error) => {
            report.log(&format!(
                "无法校验设备身份（{error}）；本轮只在本机生效，不再上传。"
            ));
            Ok(false)
        }
    }
}

/// 合并单个登记文件（§5）。`Others/` 里未登记的文件不会被写回本地（§4.1）。
fn merge_registered(
    layout: &Layout,
    index: &mut SyncIndex,
    relative: &str,
    report: &dyn Reporter,
) -> Result<FileAction> {
    let mode = MergeMode::for_path(relative)?;
    let local = layout.local_source(relative);
    let others = layout.others_path(relative);
    for path in [&local, &others] {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
    }

    match mode {
        MergeMode::DictUnion | MergeMode::KeyUnion => {
            union_file(index, relative, mode, &local, &others, report)
        }
        MergeMode::Newest => newest_file(layout, index, relative, &local, &others, report),
    }
}

fn union_file(
    index: &mut SyncIndex,
    relative: &str,
    mode: MergeMode,
    local: &Path,
    others: &Path,
    report: &dyn Reporter,
) -> Result<FileAction> {
    match (local.is_file(), others.is_file()) {
        (false, false) => Ok(FileAction::Unchanged),
        (false, true) => {
            fs::copy(others, local)?;
            record(index, relative, local)?;
            report.log(&format!(
                "{relative}：本地缺失，已从 Others/ 补全（{}）。",
                mode.label()
            ));
            Ok(FileAction::PulledFromOthers)
        }
        (true, false) => {
            fs::copy(local, others)?;
            record(index, relative, local)?;
            report.log(&format!("{relative}：Others/ 缺失，已上传本地版本。"));
            Ok(FileAction::PushedToOthers)
        }
        (true, true) => {
            let local_text = read_utf8(local)?;
            let others_text = read_utf8(others)?;
            let merged = merge_text_for_path(relative, mode, &local_text, &others_text)
                .context("并集合并未返回结果")?;
            if merged == local_text && merged == others_text {
                record(index, relative, local)?;
                index.conflicts.remove(relative);
                return Ok(FileAction::Unchanged);
            }
            if merged != local_text {
                fs::write(local, &merged)?;
            }
            if merged != others_text {
                fs::write(others, &merged)?;
            }
            record(index, relative, local)?;
            index.conflicts.remove(relative);
            report.log(&format!(
                "{relative}：按「{}」合并并写回本地源与 Others/。",
                mode.label()
            ));
            Ok(FileAction::Merged)
        }
    }
}

/// newest 型文件必须配三向判断，否则两端都会认为"我更新"而互相覆盖（§5.1）。
fn newest_file(
    layout: &Layout,
    index: &mut SyncIndex,
    relative: &str,
    local: &Path,
    others: &Path,
    report: &dyn Reporter,
) -> Result<FileAction> {
    let local_state = transport::state_of(local)?;
    let others_state = transport::state_of(others)?;
    let base = index.files.get(relative).cloned();

    if local_state.is_none() && others_state.is_none() {
        return Ok(FileAction::Unchanged);
    }
    // 本地缺失时按"从 Others/ 补全"处理，与并集型一致（§4.3）。
    if local_state.is_none() {
        fs::copy(others, local)?;
        record_if_shared_confirmed(index, relative, local)?;
        index.conflicts.remove(relative);
        report.log(&format!("{relative}：本地缺失，已用 Others/ 版本补全。"));
        return Ok(FileAction::PulledFromOthers);
    }
    if others_state.is_none() {
        fs::copy(local, others)?;
        stage_pending(index, relative, local)?;
        index.conflicts.remove(relative);
        report.log(&format!("{relative}：Others/ 缺失，已上传本地版本。"));
        return Ok(FileAction::PushedToOthers);
    }

    let local_state = local_state.context("本地文件刚刚消失")?;
    let others_state = others_state.context("Others 文件刚刚消失")?;
    if local_state.sha256 == others_state.sha256 {
        if index
            .remote
            .get(relative)
            .and_then(|mark| mark.sha256.as_deref())
            == Some(local_state.sha256.as_str())
        {
            index.files.insert(relative.to_owned(), local_state);
            index.pending_files.remove(relative);
        } else {
            index.pending_files.insert(relative.to_owned(), local_state);
        }
        index.conflicts.remove(relative);
        return Ok(FileAction::Unchanged);
    }

    let local_changed = differs_from_base(&local_state, base.as_ref());
    let others_changed = differs_from_base(&others_state, base.as_ref());

    match (base.is_some(), local_changed, others_changed) {
        (true, true, false) => {
            fs::copy(local, others)?;
            stage_pending(index, relative, local)?;
            index.conflicts.remove(relative);
            report.log(&format!("{relative}：本机较新，已覆盖 Others/。"));
            Ok(FileAction::PushedToOthers)
        }
        (true, false, true) => {
            fs::copy(others, local)?;
            record_if_shared_confirmed(index, relative, local)?;
            index.conflicts.remove(relative);
            report.log(&format!("{relative}：他机较新，已覆盖本地源文件。"));
            Ok(FileAction::PulledFromOthers)
        }
        _ => resolve_conflict(
            layout,
            index,
            relative,
            others,
            &others_state.sha256,
            report,
        ),
    }
}

/// 冲突：保留本地，远端副本另存为 `<name>.conflict-<ts>.<ext>` 并提示（§5.1）。
/// 同一份远端内容只提示一次，避免每轮都生成新副本。
fn resolve_conflict(
    layout: &Layout,
    index: &mut SyncIndex,
    relative: &str,
    others: &Path,
    remote_hash: &str,
    report: &dyn Reporter,
) -> Result<FileAction> {
    if index.conflicts.get(relative).map(String::as_str) == Some(remote_hash) {
        report.log(&format!(
            "{relative}：仍处于冲突状态，已保留本地版本，未重复生成副本，请手动处理。"
        ));
        return Ok(FileAction::Conflict(relative.to_owned()));
    }
    let copy_relative = identity::conflict_path(relative);
    let copy_path = layout.local_source(&copy_relative);
    if let Some(parent) = copy_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(others, &copy_path)?;
    index
        .conflicts
        .insert(relative.to_owned(), remote_hash.to_owned());
    // 刻意**不**更新基线：若把基线改成"本地当前内容"，下一轮本地会被判成"未改动"、
    // 远端"已改动"，从而走进 (true, false, true) 分支用远端静默覆盖本地（§5.1）。
    // 保留旧基线，本地下一轮仍判为已改动，会再次命中这里做去重提示而不是覆盖。
    report.log(&format!(
        "{relative}：检测到双端修改冲突。已保留本地版本，远端版本另存为 {copy_relative}，请手动比较后决定保留哪一份。"
    ));
    Ok(FileAction::Conflict(copy_relative))
}

fn differs_from_base(state: &IndexEntry, base: Option<&IndexEntry>) -> bool {
    match base {
        Some(base) => state.sha256 != base.sha256,
        None => true,
    }
}

fn record(index: &mut SyncIndex, relative: &str, source: &Path) -> Result<()> {
    match transport::state_of(source)? {
        Some(state) => {
            index.files.insert(relative.to_owned(), state);
        }
        None => {
            index.files.remove(relative);
        }
    }
    Ok(())
}

fn record_if_shared_confirmed(index: &mut SyncIndex, relative: &str, source: &Path) -> Result<()> {
    if let Some(state) = transport::state_of(source)? {
        if index
            .remote
            .get(relative)
            .and_then(|mark| mark.sha256.as_deref())
            == Some(state.sha256.as_str())
        {
            index.files.insert(relative.to_owned(), state);
            index.pending_files.remove(relative);
        } else {
            index.pending_files.insert(relative.to_owned(), state);
        }
    }
    Ok(())
}

fn stage_pending(index: &mut SyncIndex, relative: &str, source: &Path) -> Result<()> {
    if let Some(state) = transport::state_of(source)? {
        index.pending_files.insert(relative.to_owned(), state);
    }
    Ok(())
}

/// librime 可能把方案资源（如万象 `.gram`）一起复制进快照目录，`/sync` 之后必须清掉，
/// 否则会被当成 userdb 快照上传（§8.2：二进制模型不同步）。
fn purge_snapshot_dir(context: &Context, list: &SyncList, report: &dyn Reporter) -> Result<()> {
    let dir = &context.layout.device;
    if !dir.is_dir() {
        return Ok(());
    }
    let mut removed = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let lower = name.to_ascii_lowercase();
        let is_leveldb = entry.file_type()?.is_dir() && lower.ends_with(".userdb");
        if !crate::paths::is_binary_scheme_resource(&name) && !is_leveldb {
            continue;
        }
        if list.contains(&name) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
        removed.push(name);
    }
    if !removed.is_empty() {
        report.log(&format!(
            "已从快照目录移除非 userdb 文件：{}",
            removed.join("、")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{report::NullReporter, transport::state_of};

    fn setup(root: &Path, id: &str) -> Layout {
        let layout = Layout::new(root.join("Rime"), id);
        layout.ensure_dirs().unwrap();
        layout
    }

    fn baseline(path: &Path) -> IndexEntry {
        state_of(path).unwrap().expect("基线文件必须存在")
    }

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, text).unwrap();
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    fn conflict_copies(layout: &Layout) -> Vec<String> {
        transport::walk_files(&layout.rime)
            .unwrap()
            .into_iter()
            .map(|(relative, _)| relative)
            .filter(|relative| relative.contains(".conflict-"))
            .collect()
    }

    #[test]
    fn three_way_detects_only_side_that_changed() {
        let base = IndexEntry {
            sha256: "aaa".into(),
            size: 1,
            mtime: 0,
        };
        let same = base.clone();
        let other = IndexEntry {
            sha256: "bbb".into(),
            size: 1,
            mtime: 0,
        };
        assert!(!differs_from_base(&same, Some(&base)));
        assert!(differs_from_base(&other, Some(&base)));
        // 没有基线时任何内容都算"已改动"。
        assert!(differs_from_base(&same, None));
    }

    #[test]
    fn union_writes_the_same_result_to_local_source_and_others() {
        let dir = tempfile::tempdir().unwrap();
        let layout = setup(dir.path(), "pc01");
        let local = layout.local_source("my_words.dict.yaml");
        let others = layout.others_path("my_words.dict.yaml");
        write(&local, "---\nname: my\n...\n甲\tjia\t10\n");
        write(&others, "---\nname: my\n...\n乙\tyi\t20\n");
        let mut index = SyncIndex::default();

        let action =
            merge_registered(&layout, &mut index, "my_words.dict.yaml", &NullReporter).unwrap();

        assert_eq!(action, FileAction::Merged);
        assert_eq!(read(&local), read(&others));
        assert!(read(&local).contains("甲\tjia\t10"));
        assert!(read(&local).contains("乙\tyi\t20"));
        assert_eq!(
            index
                .files
                .get("my_words.dict.yaml")
                .map(|e| e.sha256.clone()),
            Some(baseline(&local).sha256)
        );
        // 幂等：再合并一次不再产生变化。
        assert_eq!(
            merge_registered(&layout, &mut index, "my_words.dict.yaml", &NullReporter).unwrap(),
            FileAction::Unchanged
        );
    }

    #[test]
    fn newest_pushes_local_change_when_others_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let layout = setup(dir.path(), "pc01");
        let local = layout.local_source("lua/date.lua");
        let others = layout.others_path("lua/date.lua");
        write(&local, "v1");
        write(&others, "v1");
        let mut index = SyncIndex::default();
        index.files.insert("lua/date.lua".into(), baseline(&local));

        write(&local, "v2");
        let action = merge_registered(&layout, &mut index, "lua/date.lua", &NullReporter).unwrap();

        assert_eq!(action, FileAction::PushedToOthers);
        assert_eq!(read(&others), "v2");
        assert_eq!(read(&local), "v2");
        assert!(conflict_copies(&layout).is_empty());
    }

    #[test]
    fn unpublished_newest_changes_keep_the_previous_baseline_including_first_sync() {
        for has_base in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let layout = setup(dir.path(), "pc01");
            let relative = "default.custom.yaml";
            let local = layout.local_source(relative);
            let others = layout.others_path(relative);
            let mut index = SyncIndex::default();
            if has_base {
                write(&local, "v1");
                write(&others, "v1");
                index.files.insert(relative.into(), baseline(&local));
            }
            write(&local, "unpublished-v2");
            merge_registered(&layout, &mut index, relative, &NullReporter).unwrap();
            // 再次手动同步时两份本地文件相同，也不能提前承认远端已保存。
            merge_registered(&layout, &mut index, relative, &NullReporter).unwrap();
            assert_eq!(index.files.contains_key(relative), has_base);
            if has_base {
                assert_eq!(index.files[relative].sha256, crate::hash::sha256_hex(b"v1"));
            }
            index.save(&layout.index_file()).unwrap();
            let mut index = SyncIndex::load(&layout.index_file()).unwrap();
            // 模拟上传失败/412 后，下一轮搬运拉到他机的新内容。
            write(&others, "remote-v3");
            let action = merge_registered(&layout, &mut index, relative, &NullReporter).unwrap();
            assert!(matches!(action, FileAction::Conflict(_)));
            assert_eq!(read(&local), "unpublished-v2");
            assert_eq!(conflict_copies(&layout).len(), 1);
        }
    }

    #[test]
    fn failed_session_read_disables_all_manual_uploads() {
        use crate::webdav::test_support::{Reply, serve};
        let server = serve(|_| Reply::new(500, Vec::new()));
        let dir = tempfile::tempdir().unwrap();
        let mut context = Context::for_test(dir.path(), "pc01");
        context.layout.ensure_dirs().unwrap();
        context.dav = crate::webdav::WebDav::new(&server.url, "user", "pass").unwrap();
        fs::write(
            context.layout.device.join("words.userdb.txt"),
            b"old owner words",
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        struct StopBeforeRime<'a>(&'a AtomicBool);
        impl Reporter for StopBeforeRime<'_> {
            fn log(&self, _: &str) {}
            fn progress(&self, progress: u8) {
                if progress == 60 {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
        }
        assert!(manual_sync(&context, &cancel, &StopBeforeRime(&cancel)).is_err());
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert!(requests[0].path.ends_with("/owner.json"));
    }

    #[test]
    fn newest_pulls_remote_change_when_local_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let layout = setup(dir.path(), "pc01");
        let local = layout.local_source("default.custom.yaml");
        let others = layout.others_path("default.custom.yaml");
        write(&local, "v1");
        write(&others, "v1");
        let mut index = SyncIndex::default();
        index
            .files
            .insert("default.custom.yaml".into(), baseline(&local));

        write(&others, "v2-remote");
        let action =
            merge_registered(&layout, &mut index, "default.custom.yaml", &NullReporter).unwrap();

        assert_eq!(action, FileAction::PulledFromOthers);
        assert_eq!(read(&local), "v2-remote");
        assert_eq!(read(&others), "v2-remote");
        assert!(conflict_copies(&layout).is_empty());
    }

    #[test]
    fn newest_conflict_keeps_local_and_saves_one_remote_copy() {
        let dir = tempfile::tempdir().unwrap();
        let layout = setup(dir.path(), "pc01");
        let local = layout.local_source("lua/date.lua");
        let others = layout.others_path("lua/date.lua");
        write(&local, "base");
        write(&others, "base");
        let mut index = SyncIndex::default();
        index.files.insert("lua/date.lua".into(), baseline(&local));

        write(&local, "local-edit");
        write(&others, "remote-edit");
        let first = merge_registered(&layout, &mut index, "lua/date.lua", &NullReporter).unwrap();

        let copies = conflict_copies(&layout);
        assert!(matches!(first, FileAction::Conflict(_)));
        assert_eq!(copies.len(), 1, "冲突时应生成恰好一个副本: {copies:?}");
        assert_eq!(read(&layout.local_source(&copies[0])), "remote-edit");
        // 不静默覆盖：本地与远端都保持原样。
        assert_eq!(read(&local), "local-edit");
        assert_eq!(read(&others), "remote-edit");

        // 同一份远端内容不会每轮都生成新副本。
        merge_registered(&layout, &mut index, "lua/date.lua", &NullReporter).unwrap();
        assert_eq!(conflict_copies(&layout).len(), 1);
        // 关键：第二轮的再判定不能把本地版本静默覆盖成远端内容。
        assert_eq!(read(&local), "local-edit");
        assert_eq!(read(&others), "remote-edit");
    }

    #[test]
    fn deleted_local_source_is_refilled_from_others() {
        let dir = tempfile::tempdir().unwrap();
        let layout = setup(dir.path(), "pc01");
        let others = layout.others_path("custom_phrase.txt");
        write(&others, "你好\tni hao\t100\n");
        let mut index = SyncIndex::default();

        let action =
            merge_registered(&layout, &mut index, "custom_phrase.txt", &NullReporter).unwrap();

        assert_eq!(action, FileAction::PulledFromOthers);
        assert_eq!(
            read(&layout.local_source("custom_phrase.txt")),
            read(&others)
        );
    }

    #[test]
    fn purge_removes_binary_resources_from_snapshot_dir() {
        let dir = tempfile::tempdir().unwrap();
        let context = crate::context::Context::for_test(dir.path(), "pc01");
        context.layout.ensure_dirs().unwrap();
        fs::write(
            context.layout.device.join("wanxiang-lts-zh-hans.gram"),
            b"binary",
        )
        .unwrap();
        fs::write(context.layout.device.join("rime.userdb.txt"), b"a\tb\t1\n").unwrap();

        purge_snapshot_dir(&context, &SyncList::default(), &crate::report::NullReporter).unwrap();

        assert!(
            !context
                .layout
                .device
                .join("wanxiang-lts-zh-hans.gram")
                .exists()
        );
        assert!(context.layout.device.join("rime.userdb.txt").exists());
    }
}
