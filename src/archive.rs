use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

/// v0.2.5 及更早版本把全部同步数据打成一个压缩包放在同步根目录。
pub const ARCHIVE_NAME: &str = "webdav-sync.zip";

/// 解压旧版压缩包，用于一次性迁移到 `Sync/Others/` 的散文件布局（§11）。
pub fn extract(archive: &Path, root: &Path) -> Result<Vec<String>> {
    fs::create_dir_all(root)?;
    let file = fs::File::open(archive)
        .with_context(|| format!("打开旧版压缩包 {} 失败", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file).context("旧版压缩包格式无法识别")?;
    let mut extracted = Vec::new();
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_owned();
        // installation.yaml 是 RIME 的设备安装记录，不属于共享词库内容；
        // 本地 copy_tree 与远端散文件迁移都会剔除它，这里保持一致。
        if Path::new(&name)
            .file_name()
            .is_some_and(|file| file == "installation.yaml")
        {
            continue;
        }
        let target = safe_local(root, &name)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = fs::File::create(&target)?;
        std::io::copy(&mut entry, &mut out)?;
        extracted.push(name);
    }
    Ok(extracted)
}

/// 防止压缩包条目通过 `..` 或绝对路径逃逸到目标目录之外（zip-slip）。
fn safe_local(root: &Path, relative: &str) -> Result<PathBuf> {
    let mut out = root.to_path_buf();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(name) => out.push(name),
            _ => bail!("压缩包包含非法路径: {relative}"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    #[test]
    fn extract_writes_entries_and_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("old.zip");
        {
            let file = fs::File::create(&archive).unwrap();
            let mut writer = ZipWriter::new(file);
            let options =
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            writer.start_file("custom_phrase.txt", options).unwrap();
            std::io::Write::write_all(&mut writer, b"hello").unwrap();
            writer.start_file("../evil.txt", options).unwrap();
            std::io::Write::write_all(&mut writer, b"nope").unwrap();
            writer.start_file("installation.yaml", options).unwrap();
            std::io::Write::write_all(&mut writer, b"placeholder").unwrap();
            writer.finish().unwrap();
        }
        let root = dir.path().join("out");
        assert!(extract(&archive, &root).is_err() || !root.join("../evil.txt").exists());
        assert_eq!(
            fs::read_to_string(root.join("custom_phrase.txt")).unwrap(),
            "hello"
        );
        // installation.yaml 属于设备安装记录，迁移时应被剔除，不能落进 Others/。
        assert!(!root.join("installation.yaml").exists());
    }

    #[test]
    fn safe_local_rejects_absolute_paths() {
        let root = Path::new("/tmp/root");
        assert!(safe_local(root, "/abs").is_err());
        assert!(safe_local(root, "ok/file").is_ok());
    }
}
