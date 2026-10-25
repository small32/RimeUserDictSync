use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// 原子地写入文本文件：先写同目录临时文件并 `sync`，再 `rename` 覆盖。
///
/// `fs::write` 是**原地覆写**：写到一半崩溃或断电会留下一份被截断的文件。
/// `RIME/custom_phrase.txt` 是用户自定义短语的唯一来源，一旦写坏就是数据丢失，
/// 所以凡写回用户词库都必须走这里。
pub fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let temp = temp_sibling(path);
    let result = (|| -> Result<()> {
        let mut file = fs::File::create(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        Ok(fs::rename(&temp, path)?)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.with_context(|| format!("写入 {} 失败", path.display()))
}

/// 原子地覆盖目标文件：先复制到同目录临时文件，再 `rename` 覆盖。
///
/// `fs::copy` 到已存在的路径同样是原地覆写，崩溃会留下截断文件；
/// 这里与 [`write_atomic`] 保持同一策略。
pub fn copy_atomic(from: &Path, to: &Path) -> Result<()> {
    let temp = temp_sibling(to);
    let result = (|| -> Result<()> {
        fs::copy(from, &temp)?;
        Ok(fs::rename(&temp, to)?)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.with_context(|| format!("覆盖 {} 失败", to.display()))
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    to_hex(&hasher.finalize())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("读取 {} 失败", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(to_hex(&hasher.finalize()))
}

/// 文件大小的便捷封装，读取失败时返回 0。
pub fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// 以秒为单位的 mtime；无法读取时返回 0。
pub fn mtime_secs(path: &Path) -> i64 {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

pub fn system_time_secs(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// 在目标文件旁生成一个不冲突的临时文件路径。
pub fn temp_sibling(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tmp".into());
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temp = format!(".{name}.{stamp}.tmp");
    match target.parent() {
        Some(parent) => parent.join(temp),
        None => PathBuf::from(temp),
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// 日志里展示用的短哈希（按字符取前 8 个，避免非 ASCII 内容被按字节切断而 panic）。
pub fn short(hash: &str) -> &str {
    match hash.char_indices().nth(8) {
        Some((index, _)) => &hash[..index],
        None => hash,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 原子写必须只留下目标文件本身：临时文件不能残留在 `RIME/` 或 `Others/` 里，
    /// 否则下一轮会被当成正常的词库文件搬来搬去。
    #[test]
    fn write_atomic_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("custom_phrase.txt");
        write_atomic(&target, "甲\tjia\t1\n").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "甲\tjia\t1\n");

        write_atomic(&target, "甲\tjia\t2\n").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "甲\tjia\t2\n");

        let leftovers: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件：{leftovers:?}");
    }

    #[test]
    fn copy_atomic_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.txt");
        fs::write(&source, "new\n").unwrap();
        let target = dir.path().join("target.txt");
        fs::write(&target, "old\n").unwrap();

        copy_atomic(&source, &target).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new\n");

        let leftovers: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件：{leftovers:?}");
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn temp_sibling_stays_next_to_target() {
        let path = Path::new("a/b/c.dict.yaml");
        let temp = temp_sibling(path);
        assert_eq!(temp.parent(), Some(Path::new("a/b")));
        assert_ne!(temp, path.to_path_buf());
    }
}
