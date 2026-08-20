use anyhow::{Context, Result, bail};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

pub fn merge_file(a: &Path, b: &Path) -> Result<()> {
    if let Some(parent) = a.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = b.parent() {
        fs::create_dir_all(parent)?;
    }
    match (a.is_file(), b.is_file()) {
        (false, false) => {}
        (false, true) => {
            fs::copy(b, a)?;
        }
        (true, false) => {
            fs::copy(a, b)?;
        }
        (true, true) => {
            let merged = dictionary_union(a, b)?;
            fs::write(a, &merged)?;
            fs::write(b, merged)?;
        }
    }
    Ok(())
}

fn dictionary_union(a: &Path, b: &Path) -> Result<String> {
    let a = read_utf8(a)?;
    let b = read_utf8(b)?;
    let al: Vec<_> = a.lines().collect();
    let bl: Vec<_> = b.lines().collect();
    let ah = header_end(&al)?;
    let bh = header_end(&bl)?;
    let mut out: Vec<String> = al[..ah].iter().map(|s| (*s).to_owned()).collect();
    let mut exact = HashSet::new();
    let mut weighted: HashMap<String, (f64, usize)> = HashMap::new();
    for line in al[ah..].iter().chain(&bl[bh..]) {
        if let Some((key, weight)) = parse_weight(line) {
            if let Some((old, pos)) = weighted.get(&key).copied() {
                if weight > old {
                    out[pos] = (*line).to_owned();
                    weighted.insert(key, (weight, pos));
                }
            } else {
                let pos = out.len();
                out.push((*line).to_owned());
                weighted.insert(key, (weight, pos));
            }
        } else if exact.insert((*line).to_owned()) {
            out.push((*line).to_owned());
        }
    }
    Ok(if out.is_empty() {
        String::new()
    } else {
        out.join("\n") + "\n"
    })
}

fn read_utf8(path: &Path) -> Result<String> {
    let bytes = fs::read(path)?;
    if bytes.contains(&0) {
        bail!("词库目录含二进制文件: {}", path.display());
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    String::from_utf8(bytes.to_vec())
        .with_context(|| format!("词库文件不是 UTF-8: {}", path.display()))
}
fn header_end(lines: &[&str]) -> Result<usize> {
    if lines.first().map(|s| s.trim()) != Some("---") {
        return Ok(0);
    }
    lines
        .iter()
        .position(|s| s.trim() == "...")
        .map(|i| i + 1)
        .context("词库文件头以 --- 开始但缺少 ...")
}
fn parse_weight(line: &str) -> Option<(String, f64)> {
    let (key, number) = line.rsplit_once('\t')?;
    Some((key.to_owned(), number.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn larger_weight_wins() {
        assert_eq!(parse_weight("𤭢\tcei\t1000").unwrap().1, 1000.0);
    }
    #[test]
    fn union_preserves_header_and_max_weight() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.dict.yaml");
        let b = dir.path().join("b.dict.yaml");
        fs::write(&a, "---\nname: test\n...\n甲\tjia\t10\n乙\tyi\t3\n").unwrap();
        fs::write(&b, "---\nname: test\n...\n甲\tjia\t20\n丙\tbing\t5\n").unwrap();
        let merged = dictionary_union(&a, &b).unwrap();
        assert!(merged.starts_with("---\nname: test\n...\n"));
        assert!(merged.contains("甲\tjia\t20"));
        assert!(!merged.contains("甲\tjia\t10"));
        assert!(merged.contains("乙\tyi\t3"));
        assert!(merged.contains("丙\tbing\t5"));
    }
}
