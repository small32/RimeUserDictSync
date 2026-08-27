use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::SystemTime,
};
use time::OffsetDateTime;
use walkdir::WalkDir;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter, write::SimpleFileOptions};

/// WebDAV 远端存储的固定压缩包文件名。
pub const ARCHIVE_NAME: &str = "webdav-sync.zip";

/// 把 root 目录树打包为 Deflated 的 zip，文件条目保留原始 mtime。
pub fn pack(root: &Path, archive: &Path) -> Result<()> {
    if let Some(parent) = archive.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::File::create(archive)?;
    let mut writer = ZipWriter::new(file);
    let base = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        let rel = entry
            .path()
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| DateTime::try_from(OffsetDateTime::from(t)).ok());
        let mut options = base;
        if let Some(t) = mtime {
            options = options.last_modified_time(t);
        }
        writer.start_file(rel.clone(), options).with_context(|| {
            format!("写入压缩包条目 {rel}")
        })?;
        let mut file = fs::File::open(entry.path())?;
        std::io::copy(&mut file, &mut writer)?;
    }
    writer.finish()?;
    Ok(())
}

/// 把 zip 解压到 root 目录，恢复条目 mtime；对越界路径直接报错。
pub fn unpack(archive: &Path, root: &Path) -> Result<()> {
    fs::create_dir_all(root)?;
    let file = fs::File::open(archive)?;
    let mut zip = ZipArchive::new(file)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let target = safe_local(root, entry.name())?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = fs::File::create(&target)?;
        std::io::copy(&mut entry, &mut out)?;
        if let Some(dt) = entry.last_modified() {
            if let Ok(t) = OffsetDateTime::try_from(dt) {
                let st: SystemTime = t.into();
                filetime::set_file_mtime(&target, filetime::FileTime::from_system_time(st))?;
            }
        }
    }
    Ok(())
}

/// 防止 zip 条目通过 `..` 或绝对路径逃逸到目标目录之外（zip-slip）。
fn safe_local(root: &Path, relative: &str) -> Result<PathBuf> {
    let mut out = root.to_path_buf();
    for c in Path::new(relative).components() {
        match c {
            Component::Normal(x) => out.push(x),
            _ => bail!("压缩包包含非法路径: {relative}"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_files_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("src");
        fs::create_dir_all(root.join("nested")).unwrap();
        let inner = root.join("nested/inner.txt");
        fs::write(&inner, b"hello inner").unwrap();
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        filetime::set_file_mtime(&inner, filetime::FileTime::from_system_time(mtime)).unwrap();

        let archive = dir.path().join("pack.zip");
        pack(&root, &archive).unwrap();

        let out_root = dir.path().join("out");
        unpack(&archive, &out_root).unwrap();

        assert_eq!(
            fs::read(out_root.join("nested/inner.txt")).unwrap(),
            b"hello inner"
        );
        let got = filetime::FileTime::from_last_modification_time(
            &fs::metadata(out_root.join("nested/inner.txt")).unwrap(),
        );
        assert_eq!(got, filetime::FileTime::from_system_time(mtime));
    }

    #[test]
    fn unpack_rejects_traversal_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("out");
        fs::create_dir_all(&root).unwrap();
        assert!(safe_local(&root, "../evil").is_err());
        assert!(safe_local(&root, "/abs").is_err());
        assert!(safe_local(&root, "ok/file").is_ok());
    }
}