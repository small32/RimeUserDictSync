use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

/// §5 的合并规则（按文件类型自动判定，不再由用户手选）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMode {
    /// `*.dict.yaml`：词条级并集，同 key 取大权重。
    DictUnion,
    /// `*.txt`：按首列 key 并集（custom_phrase / opencc 词典）。
    KeyUnion,
    /// 其余（`*.yaml` 非 dict、`*.json`、`*.lua` 等）：只能 newest。
    Newest,
}

impl MergeMode {
    pub fn for_path(relative: &str) -> Result<Self> {
        let lower = relative.to_ascii_lowercase();
        if lower.ends_with(".userdb.txt") {
            bail!("userdb 快照不能加入同步列表，它由 RIME 的 /sync 与 Sync/<ID>/ 负责");
        }
        if lower.ends_with(".dict.yaml") {
            Ok(Self::DictUnion)
        } else if lower.ends_with(".txt") {
            Ok(Self::KeyUnion)
        } else {
            // 脚本与 JSON 没有合并语义，只能 newest。
            Ok(Self::Newest)
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::DictUnion => "词条并集",
            Self::KeyUnion => "首列并集",
            Self::Newest => "按时间(newest)",
        }
    }
}

/// 文本合并入口：并集类返回合并结果，newest 类返回 `None`。
pub fn merge_text(mode: MergeMode, local: &str, remote: &str) -> Option<String> {
    match mode {
        MergeMode::DictUnion => Some(dict_union(local, remote)),
        MergeMode::KeyUnion => Some(key_union(local, remote)),
        MergeMode::Newest => None,
    }
}

/// `*.dict.yaml` 词条级并集：表头取本地（缺失则取远端），词条按 key 去重取大权重。
pub fn dict_union(local: &str, remote: &str) -> String {
    let local_lines: Vec<&str> = local.lines().collect();
    let remote_lines: Vec<&str> = remote.lines().collect();
    let local_header = header_end(&local_lines);
    let remote_header = header_end(&remote_lines);
    let (header, local_body, remote_body) = if local_header.is_some() || !local_lines.is_empty() {
        (
            local_lines[..local_header.unwrap_or(0)].to_vec(),
            &local_lines[local_header.unwrap_or(0)..],
            &remote_lines[remote_header.unwrap_or(0)..],
        )
    } else {
        (
            remote_lines[..remote_header.unwrap_or(0)].to_vec(),
            &local_lines[0..0],
            &remote_lines[remote_header.unwrap_or(0)..],
        )
    };

    let mut out: Vec<String> = header.iter().map(|line| (*line).to_owned()).collect();
    let mut exact: HashSet<String> = HashSet::new();
    let mut weighted: HashMap<String, (f64, usize)> = HashMap::new();
    for line in local_body.iter().chain(remote_body.iter()) {
        if let Some((key, weight)) = parse_weight(line) {
            match weighted.get(&key).copied() {
                Some((old_weight, position)) if weight > old_weight => {
                    out[position] = (*line).to_owned();
                    weighted.insert(key, (weight, position));
                }
                Some(_) => {}
                None => {
                    let position = out.len();
                    out.push((*line).to_owned());
                    weighted.insert(key, (weight, position));
                }
            }
        } else if exact.insert((*line).to_owned()) {
            out.push((*line).to_owned());
        }
    }
    join_lines(out)
}

/// `*.txt` 按首列 key 并集（自定义短语 / opencc 转换表）。
///
/// 同一 key 出现两次时：两侧都有可解析的数字权重就取大的（自定义短语的权重列），
/// 否则保留先出现的一条 —— 对 `原词\t转换词` 这类两列表，本地内容优先且结果确定。
pub fn key_union(local: &str, remote: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut keys: HashMap<String, usize> = HashMap::new();
    let mut seen_plain: HashSet<String> = HashSet::new();
    for line in local.lines().chain(remote.lines()) {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.trim().is_empty() || trimmed.starts_with('#') {
            if seen_plain.insert(trimmed.to_owned()) {
                out.push(trimmed.to_owned());
            }
            continue;
        }
        let key = trimmed
            .split('\t')
            .next()
            .unwrap_or(trimmed)
            .trim()
            .to_owned();
        match keys.get(&key).copied() {
            Some(position) => {
                let existing = out[position].clone();
                if let (Some(new_weight), Some(old_weight)) =
                    (weight_of(trimmed), weight_of(&existing))
                    && new_weight > old_weight
                {
                    out[position] = trimmed.to_owned();
                }
            }
            None => {
                keys.insert(key, out.len());
                out.push(trimmed.to_owned());
            }
        }
    }
    join_lines(out)
}

fn join_lines(lines: Vec<String>) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        lines.join("\n") + "\n"
    }
}

/// 读取文本文件：拒绝二进制内容，容忍 UTF-8 BOM。
pub fn read_utf8(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("读取 {} 失败", path.display()))?;
    if bytes.contains(&0) {
        bail!("文件含二进制内容，不能做文本合并: {}", path.display());
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    String::from_utf8(bytes.to_vec()).with_context(|| format!("文件不是 UTF-8: {}", path.display()))
}

/// 表头 `---` … `...` 的结束位置；没有表头时返回 0。
fn header_end(lines: &[&str]) -> Option<usize> {
    if lines.first().map(|line| line.trim()) != Some("---") {
        return None;
    }
    lines
        .iter()
        .position(|line| line.trim() == "...")
        .map(|i| i + 1)
}

/// dict 行：`key<TAB>...<TAB>weight`，key 为最后一个 Tab 之前的全部内容。
fn parse_weight(line: &str) -> Option<(String, f64)> {
    let (key, number) = line.rsplit_once('\t')?;
    Some((key.to_owned(), number.trim().parse().ok()?))
}

/// `*.txt` 行的末尾数字权重（没有则 `None`）。
fn weight_of(line: &str) -> Option<f64> {
    let mut fields = line.split('\t');
    let last = fields.next_back()?;
    last.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_is_derived_from_extension() {
        assert_eq!(
            MergeMode::for_path("my_words.dict.yaml").unwrap(),
            MergeMode::DictUnion
        );
        assert_eq!(
            MergeMode::for_path("cn_dicts/base.dict.yaml").unwrap(),
            MergeMode::DictUnion
        );
        assert_eq!(
            MergeMode::for_path("custom_phrase.txt").unwrap(),
            MergeMode::KeyUnion
        );
        assert_eq!(
            MergeMode::for_path("default.custom.yaml").unwrap(),
            MergeMode::Newest
        );
        assert_eq!(MergeMode::for_path("lua/x.lua").unwrap(), MergeMode::Newest);
        assert_eq!(
            MergeMode::for_path("opencc/t2s.json").unwrap(),
            MergeMode::Newest
        );
        assert!(MergeMode::for_path("wanxiang.userdb.txt").is_err());
    }

    #[test]
    fn dict_union_keeps_header_and_max_weight() {
        let local = "---\nname: test\n...\n甲\tjia\t10\n乙\tyi\t3\n";
        let remote = "---\nname: test\n...\n甲\tjia\t20\n丙\tbing\t5\n";
        let merged = dict_union(local, remote);
        assert!(merged.starts_with("---\nname: test\n...\n"));
        assert!(merged.contains("甲\tjia\t20"));
        assert!(!merged.contains("甲\tjia\t10"));
        assert!(merged.contains("乙\tyi\t3"));
        assert!(merged.contains("丙\tbing\t5"));
        // 幂等：再合并一次结果不变。
        assert_eq!(dict_union(&merged, &merged), merged);
    }

    #[test]
    fn key_union_dedupes_by_first_column() {
        let local = "你好\tni hao\t100\n";
        let remote = "你好\tni hao\t200\n世界\tshi jie\t50\n";
        let merged = key_union(local, remote);
        assert!(merged.contains("你好\tni hao\t200"));
        assert!(!merged.contains("你好\tni hao\t100"));
        assert!(merged.contains("世界\tshi jie\t50"));
    }

    #[test]
    fn key_union_keeps_local_side_for_two_column_tables() {
        let local = "裏\t里\n";
        let remote = "裏\t裡\n後\t后\n";
        let merged = key_union(local, remote);
        assert!(merged.contains("裏\t里"));
        assert!(!merged.contains("裏\t裡"));
        assert!(merged.contains("後\t后"));
    }
}
