use crate::{
    config::{Settings, SyncFileMode, validate_installation_id, yaml_scalar},
    dictionary, platform,
    webdav::WebDav,
};
use anyhow::{Context, Result, bail};
use chrono::Local;
use regex::Regex;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};
use walkdir::WalkDir;

pub trait Reporter: Send + Sync {
    fn log(&self, text: &str);
    fn progress(&self, value: u8);
}

pub fn run(
    base_dir: &Path,
    settings: Settings,
    cancel: Arc<AtomicBool>,
    report: &dyn Reporter,
) -> Result<()> {
    report.progress(0);
    let user_dir = if settings.user_data_dir.trim().is_empty() {
        platform::default_user_dir()
    } else {
        PathBuf::from(&settings.user_data_dir)
    };
    let installation = user_dir.join("installation.yaml");
    if !installation.is_file() {
        bail!("找不到 installation.yaml，请先指定 RIME 用户词库");
    }
    let installation_id = yaml_scalar(&installation, "installation_id")?;
    validate_installation_id(&installation_id)?;
    let selected_files = settings
        .sync_files
        .iter()
        .map(|file| {
            Ok((
                validate_sync_file_path(&file.path)?,
                file.mode,
                file.writeback,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let configured_root = PathBuf::from(yaml_scalar(&installation, "sync_dir")?);
    let configured_root = if configured_root.is_absolute() {
        configured_root
    } else {
        user_dir.join(configured_root)
    };
    let fixed_root = base_dir.join("Sync");
    if normalize(&configured_root)? != normalize(&fixed_root)? {
        bail!("RIME 同步目录配置不正确，请重新指定 RIME 用户词库");
    }
    let sync_folder = fixed_root.join(&installation_id);
    let local_folder = fixed_root.join("WebDAV");
    if installation_id.eq_ignore_ascii_case("WebDAV") {
        bail!("当前设备 installation_id 不能是 WebDAV");
    }
    fs::create_dir_all(&sync_folder)?;
    fs::create_dir_all(&local_folder)?;
    let rime = platform::find_rime(&settings.deployer_path, base_dir)?;
    let dav = WebDav::new(
        required(&settings.webdav_url, "WebDAV 地址")?,
        required(&settings.username, "WebDAV 用户名")?,
        &settings.password,
    )?;

    report.log("步骤 1/7：第 1 次运行 RIME 用户资料同步。");
    platform::run(&rime, rime.sync_arg, &cancel)?;
    check(&cancel)?;
    remove_default_gram_unless_selected(&sync_folder, &selected_files)?;
    report.progress(15);
    for (relative, _, _) in &selected_files {
        copy_file_if_present(&user_dir.join(relative), &sync_folder.join(relative))?;
    }
    report.log(&format!(
        "步骤 1/7：RIME 自带同步数据已生成，并复制 {} 个自选文件到 Sync/{installation_id}。",
        selected_files.len()
    ));
    report.progress(25);

    check(&cancel)?;
    let remote = dav.list_recursive()?;
    recreate(&local_folder)?;
    if remote.is_empty() {
        report.log("步骤 2/7：WebDAV 远端为空，使用当前同步数据初始化。");
        copy_dir(&sync_folder, &local_folder)?;
        ensure_webdav_installation(&local_folder)?;
        dav.upload_directory(&local_folder)?;
    } else {
        report.log(&format!("步骤 2/7：下载 WebDAV 文件 {} 个。", remote.len()));
        dav.download(&remote, &local_folder)?;
        ensure_webdav_installation(&local_folder)?;
    }
    report.log("步骤 3/7：已确认本地文件夹与同步文件夹位于同一 Sync 根目录。");
    report.progress(45);

    check(&cancel)?;
    report.log("步骤 4/7：第 2 次运行 RIME 用户资料同步。");
    platform::run(&rime, rime.sync_arg, &cancel)?;
    remove_default_gram_unless_selected(&sync_folder, &selected_files)?;
    report.progress(65);
    for (relative, mode, _) in &selected_files {
        let local = local_folder.join(relative);
        let sync = sync_folder.join(relative);
        match mode {
            SyncFileMode::Union => dictionary::merge_file(&local, &sync)?,
            SyncFileMode::Newest => sync_newest_file(&local, &sync)?,
        }
    }
    ensure_webdav_installation(&local_folder)?;
    report.log(&format!(
        "步骤 5/7：已按设置同步 {} 个自选文件。",
        selected_files.len()
    ));
    check(&cancel)?;
    let writeback_count = writeback_selected_files(&sync_folder, &user_dir, &selected_files)?;
    report.log(&format!("步骤 5/7：已回写 {writeback_count} 个自选文件。"));
    report.log("步骤 5/7：开始重新部署 RIME。");
    platform::run(&rime, rime.deploy_arg, &cancel)?;
    report.progress(80);

    check(&cancel)?;
    report.log("步骤 6/7：覆盖上传本地文件夹全部内容到 WebDAV。");
    dav.upload_directory(&local_folder)?;
    report.progress(95);
    report.log("步骤 7/7：上传完成，清空本地文件夹和同步文件夹（保留目录）。");
    clear(&local_folder)?;
    clear(&sync_folder)?;
    report.progress(100);
    report.log("全部同步步骤已完成。");
    Ok(())
}

pub fn timestamped(text: &str) -> String {
    format!("{} {}", Local::now().format("%Y%m%d%H%M%S"), text)
}
fn required<'a>(v: &'a str, name: &str) -> Result<&'a str> {
    if v.trim().is_empty() {
        bail!("{name}不能为空")
    }
    Ok(v)
}
fn check(c: &AtomicBool) -> Result<()> {
    if c.load(Ordering::Relaxed) {
        bail!("用户已停止同步")
    }
    Ok(())
}
fn normalize(p: &Path) -> Result<PathBuf> {
    Ok(if p.exists() {
        p.canonicalize()?
    } else {
        let parent = p.parent().context("路径无父目录")?;
        fs::create_dir_all(parent)?;
        parent
            .canonicalize()?
            .join(p.file_name().context("路径无文件名")?)
    })
}
fn recreate(p: &Path) -> Result<()> {
    if p.exists() {
        fs::remove_dir_all(p)?;
    }
    fs::create_dir_all(p)?;
    Ok(())
}
fn copy_dir(a: &Path, b: &Path) -> Result<()> {
    for e in WalkDir::new(a).into_iter().collect::<Result<Vec<_>, _>>()? {
        let rel = e.path().strip_prefix(a)?;
        let dst = b.join(rel);
        if e.file_type().is_dir() {
            fs::create_dir_all(dst)?;
        } else {
            if let Some(p) = dst.parent() {
                fs::create_dir_all(p)?;
            }
            copy_file(e.path(), &dst)?;
        }
    }
    Ok(())
}
fn copy_file_if_present(a: &Path, b: &Path) -> Result<()> {
    if a.is_file() {
        if let Some(p) = b.parent() {
            fs::create_dir_all(p)?;
        }
        copy_file(a, b)?;
    }
    Ok(())
}
fn copy_file(a: &Path, b: &Path) -> Result<()> {
    fs::copy(a, b)?;
    if let Ok(modified) = fs::metadata(a)?.modified() {
        filetime::set_file_mtime(b, filetime::FileTime::from_system_time(modified))?;
    }
    Ok(())
}
fn writeback_selected_files(
    sync_folder: &Path,
    user_dir: &Path,
    selected_files: &[(PathBuf, SyncFileMode, bool)],
) -> Result<usize> {
    let mut count = 0;
    for (relative, _, writeback) in selected_files {
        if !writeback {
            continue;
        }
        let source = sync_folder.join(relative);
        if !source.is_file() {
            continue;
        }
        let target = user_dir.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        copy_file(&source, &target)?;
        count += 1;
    }
    Ok(count)
}
fn remove_default_gram_unless_selected(
    sync_folder: &Path,
    selected_files: &[(PathBuf, SyncFileMode, bool)],
) -> Result<()> {
    let gram = Path::new("wanxiang-lts-zh-hans.gram");
    let selected = selected_files.iter().any(|(relative, _, _)| {
        relative
            .to_string_lossy()
            .eq_ignore_ascii_case("wanxiang-lts-zh-hans.gram")
    });
    let generated = sync_folder.join(gram);
    if !selected && generated.is_file() {
        fs::remove_file(generated)?;
    }
    Ok(())
}
fn validate_sync_file_path(value: &str) -> Result<PathBuf> {
    let normalized = PathBuf::from(value.replace('\\', "/"));
    if value.trim().is_empty() || value.contains(';') || normalized.is_absolute() {
        bail!("同步文件路径无效: {value}");
    }
    let mut components = normalized.components();
    let first = components.next().context("同步文件路径为空")?;
    if !matches!(first, std::path::Component::Normal(_))
        || components.any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        bail!("同步文件路径不能包含 . 或 ..: {value}");
    }
    Ok(normalized)
}
fn ensure_webdav_installation(folder: &Path) -> Result<()> {
    fs::create_dir_all(folder)?;
    let path = folder.join("installation.yaml");
    let text = fs::read_to_string(&path).unwrap_or_default();
    let rx = Regex::new(r"(?m)^(\s*installation_id\s*:\s*).*$")?;
    let updated = if rx.is_match(&text) {
        rx.replace(&text, "${1}WebDAV").into_owned()
    } else {
        format!("installation_id: WebDAV\n{text}")
    };
    if updated != text {
        fs::write(path, updated)?;
    }
    Ok(())
}
fn sync_newest_file(a: &Path, b: &Path) -> Result<()> {
    if let Some(parent) = a.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = b.parent() {
        fs::create_dir_all(parent)?;
    }
    match (a.exists(), b.exists()) {
        (false, false) => {}
        (false, true) => {
            copy_file(b, a)?;
        }
        (true, false) => {
            copy_file(a, b)?;
        }
        (true, true) => {
            let at = fs::metadata(a)?
                .modified()
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let bt = fs::metadata(b)?
                .modified()
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if at > bt {
                copy_file(a, b)?;
            } else if bt > at {
                copy_file(b, a)?;
            }
        }
    }
    Ok(())
}
fn clear(root: &Path) -> Result<()> {
    fs::create_dir_all(root)?;
    for e in fs::read_dir(root)? {
        let p = e?.path();
        if p.is_dir() {
            fs::remove_dir_all(p)?;
        } else {
            fs::remove_file(p)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gram_is_removed_from_generated_sync_data_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let sync_folder = dir.path().join("Sync/device");
        fs::create_dir_all(&sync_folder).unwrap();
        let gram = sync_folder.join("wanxiang-lts-zh-hans.gram");
        fs::write(&gram, b"generated by RIME sync").unwrap();

        remove_default_gram_unless_selected(&sync_folder, &[]).unwrap();

        assert!(!gram.exists());
    }

    #[test]
    fn selected_file_is_written_back_to_its_source_path() {
        let dir = tempfile::tempdir().unwrap();
        let sync_folder = dir.path().join("Sync/device");
        let user_dir = dir.path().join("Rime");
        fs::create_dir_all(&sync_folder).unwrap();
        fs::write(sync_folder.join("default.custom.yaml"), "final content").unwrap();
        let selected_files = vec![
            (
                PathBuf::from("default.custom.yaml"),
                SyncFileMode::Newest,
                true,
            ),
            (
                PathBuf::from("no-writeback.yaml"),
                SyncFileMode::Newest,
                false,
            ),
        ];

        assert_eq!(
            writeback_selected_files(&sync_folder, &user_dir, &selected_files).unwrap(),
            1
        );
        assert_eq!(
            fs::read_to_string(user_dir.join("default.custom.yaml")).unwrap(),
            "final content"
        );
        assert!(!user_dir.join("no-writeback.yaml").exists());
    }

    #[test]
    fn newest_file_wins_and_keeps_its_modification_time() {
        let dir = tempfile::tempdir().unwrap();
        let older = dir.path().join("WebDAV/custom.bin");
        let newer = dir.path().join("device/custom.bin");
        fs::create_dir_all(older.parent().unwrap()).unwrap();
        fs::create_dir_all(newer.parent().unwrap()).unwrap();
        fs::write(&older, b"old").unwrap();
        fs::write(&newer, b"new").unwrap();
        let old_time = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        let new_time = filetime::FileTime::from_unix_time(1_800_000_000, 0);
        filetime::set_file_mtime(&older, old_time).unwrap();
        filetime::set_file_mtime(&newer, new_time).unwrap();

        sync_newest_file(&older, &newer).unwrap();

        assert_eq!(fs::read(&older).unwrap(), b"new");
        assert_eq!(
            filetime::FileTime::from_last_modification_time(&fs::metadata(&older).unwrap()),
            new_time
        );
    }
}
