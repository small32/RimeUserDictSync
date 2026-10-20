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
            Self::KeyUnion => "文本并集（三列含编码）",
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

/// 文件路径决定两列文本的语义：OpenCC 用首列作键，自定义短语包含编码列。
pub fn merge_text_for_path(
    relative: &str,
    mode: MergeMode,
    local: &str,
    remote: &str,
) -> Option<String> {
    if mode == MergeMode::KeyUnion {
        let opencc = relative
            .replace('\\', "/")
            .split('/')
            .any(|part| part.eq_ignore_ascii_case("opencc"));
        Some(key_union_with_kind(local, remote, Some(!opencc)))
    } else {
        merge_text(mode, local, remote)
    }
}

/// `*.dict.yaml` 词条级并集：表头取本地（缺失则取远端），词条按 key 去重取大权重。
pub fn dict_union(local: &str, remote: &str) -> String {
    let local_lines: Vec<&str> = local.lines().collect();
    let remote_lines: Vec<&str> = remote.lines().collect();
    let local_header = header_end(&local_lines);
    let remote_header = header_end(&remote_lines);
    // 表头取本地的；本地没有 front matter 时退回远端，否则远端 dict 的
    // name/version/columns 会被丢掉，产出一份 librime 无法加载的 *.dict.yaml。
    let header: Vec<&str> = match (local_header, remote_header) {
        (Some(end), _) => local_lines[..end].to_vec(),
        (None, Some(end)) => remote_lines[..end].to_vec(),
        (None, None) => Vec::new(),
    };
    // 没有 front matter 时整份内容都是词条体，直接从头开始。
    let local_body = &local_lines[local_header.unwrap_or(0)..];
    let remote_body = &remote_lines[remote_header.unwrap_or(0)..];

    let mut out: Vec<String> = header.iter().map(|line| (*line).to_owned()).collect();
    let mut exact: HashSet<String> = HashSet::new();
    let mut weighted: HashMap<String, (Option<f64>, usize)> = HashMap::new();
    let local_columns = dict_columns(&local_lines[..local_header.unwrap_or(0)]);
    let remote_columns = dict_columns(&remote_lines[..remote_header.unwrap_or(0)]);
    let output_columns = if local_header.is_none() && remote_header.is_some() {
        &remote_columns
    } else {
        &local_columns
    };
    for (line, columns) in local_body
        .iter()
        .map(|line| (*line, &local_columns))
        .chain(remote_body.iter().map(|line| (*line, &remote_columns)))
    {
        let line = align_dict_row(line, columns, output_columns);
        if let Some((key, weight)) = parse_dict_row(&line, output_columns) {
            match weighted.get(&key).copied() {
                Some((old_weight, position)) if weight > old_weight => {
                    out[position] = line;
                    weighted.insert(key, (weight, position));
                }
                Some(_) => {}
                None => {
                    let position = out.len();
                    out.push(line);
                    weighted.insert(key, (weight, position));
                }
            }
        } else if exact.insert(line.clone()) {
            out.push(line);
        }
    }
    join_lines(out)
}

/// 两端声明相同字段但顺序不同，输出仍必须遵守所保留表头的列顺序。
fn align_dict_row(line: &str, source: &[String], target: &[String]) -> String {
    if source == target
        || line.trim_start().starts_with('#')
        || !line.contains('\t')
        || source.len() != target.len()
        || !target.iter().all(|name| source.contains(name))
    {
        return line.to_owned();
    }
    let fields: Vec<_> = line.split('\t').collect();
    if fields.len() > source.len() {
        return line.to_owned();
    }
    let mut reordered: Vec<_> = target
        .iter()
        .map(|name| {
            let position = source.iter().position(|field| field == name).unwrap();
            fields.get(position).copied().unwrap_or("")
        })
        .collect();
    while reordered.last() == Some(&"") {
        reordered.pop();
    }
    reordered.join("\t")
}

/// `*.txt` 按首列 key 并集（自定义短语 / opencc 转换表）。
///
/// 去重键见 [`dedupe_key`]：两列按首列，三列及以上按「首列＋次列」。
/// 同一 key 出现两次时：两侧都有可解析的数字权重就取大的，
/// 否则保留先出现的一条 —— 结果确定，且绝不丢行（同 key 除外）。
pub fn key_union(local: &str, remote: &str) -> String {
    key_union_with_kind(local, remote, None)
}

fn key_union_with_kind(local: &str, remote: &str, phrase: Option<bool>) -> String {
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
        let key = match phrase {
            Some(true) => trimmed
                .split('\t')
                .take(2)
                .map(str::trim)
                .collect::<Vec<_>>()
                .join("\t"),
            Some(false) => trimmed
                .split('\t')
                .next()
                .unwrap_or(trimmed)
                .trim()
                .to_owned(),
            None => dedupe_key(trimmed),
        };
        match keys.get(&key).copied() {
            Some(position) => {
                let existing = out[position].clone();
                if phrase != Some(false)
                    && let (Some(new_weight), Some(old_weight)) =
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

/// `.txt` 一行的去重键。
///
/// - 两列（opencc 的 `原词<TAB>转换词`）：一列一键，用首列——同一个原词只能有一种转换，
///   两侧不一致时按权重/先到者取一条。
/// - 三列及以上（`custom_phrase.txt` 的 `文本<TAB>编码<TAB>权重`）：必须带上编码，
///   否则「同一个词的不同拼法」会被当成重复项删掉——这是会让用户词库悄悄变少的
///   数据丢失缺陷（同一个文本 uig / uyig / uyige 三条只能活下来一条）。
fn dedupe_key(line: &str) -> String {
    let fields: Vec<&str> = line.split('\t').collect();
    let text = fields.first().map(|field| field.trim()).unwrap_or(line);
    if fields.len() >= 3 {
        format!("{text}\t{}", fields[1].trim())
    } else {
        text.to_owned()
    }
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
    let start = lines
        .iter()
        .position(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))?;
    if lines[start].trim() != "---" {
        return None;
    }
    lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, line)| line.trim() == "...")
        .map(|(i, _)| i)
        .map(|i| i + 1)
}

fn dict_columns(header: &[&str]) -> Vec<String> {
    let Some(start) = header
        .iter()
        .position(|line| line.trim_start().starts_with("columns:"))
    else {
        return vec!["text".into(), "code".into(), "weight".into()];
    };
    let inline = header[start]
        .trim()
        .strip_prefix("columns:")
        .unwrap()
        .split('#')
        .next()
        .unwrap()
        .trim();
    let names: Vec<&str> = if inline.starts_with('[') && inline.ends_with(']') {
        inline[1..inline.len() - 1].split(',').collect()
    } else {
        header[start + 1..]
            .iter()
            .map(|line| line.trim())
            .take_while(|line| line.starts_with('-'))
            .map(|line| line[1..].trim())
            .collect()
    };
    names
        .into_iter()
        .map(|name| name.trim().trim_matches(['\'', '"']).to_owned())
        .collect()
}

fn parse_dict_row(line: &str, columns: &[String]) -> Option<(String, Option<f64>)> {
    if line.trim_start().starts_with('#') || !line.contains('\t') {
        return None;
    }
    let fields: Vec<&str> = line.split('\t').collect();
    let weight_column = columns.iter().position(|name| name == "weight");
    let weight = weight_column
        .and_then(|i| fields.get(i))
        .and_then(|number| parse_number(number));
    let key = fields
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != weight_column)
        .map(|(_, value)| value.trim())
        .collect::<Vec<_>>()
        .join("\t");
    Some((key, weight))
}

fn parse_number(value: &str) -> Option<f64> {
    value
        .trim()
        .trim_end_matches('%')
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
}

/// `*.txt` 行的末尾数字权重（没有则 `None`）。
fn weight_of(line: &str) -> Option<f64> {
    if line.split('\t').count() < 3 {
        return None;
    }
    let mut fields = line.split('\t');
    let last = fields.next_back()?;
    parse_number(last)
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
    fn dict_union_falls_back_to_the_remote_header() {
        // 本地 dict 缺 front matter、远端有：不能把远端表头丢掉。
        let local = "甲\tjia\t10\n";
        let remote = "---\nname: test\n...\n乙\tyi\t3\n";
        let merged = dict_union(local, remote);
        assert!(
            merged.starts_with("---\nname: test\n...\n"),
            "远端表头被丢了：{merged}"
        );
        assert!(merged.contains("甲\tjia\t10"));
        assert!(merged.contains("乙\tyi\t3"));
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
    fn key_union_keeps_every_code_of_the_same_phrase() {
        // custom_phrase.txt 是 `文本\t编码\t权重`：同一个词的多种拼法都必须留下，
        // 曾经按首列去重，导致 uig / uyig / uyige 只能活下来一条（P0 数据丢失）。
        let local = "有一个\tuig\t0\n有一个\tuyig\t0\n有没有\tumeiu\t0\n";
        let remote = "有一个\tuyige\t0\n有没有\tumu\t0\n世界\tshi jie\t50\n";
        let merged = key_union(local, remote);
        assert_eq!(merged.lines().count(), 6, "实际结果:\n{merged}");
        for expected in [
            "有一个\tuig\t0",
            "有一个\tuyig\t0",
            "有一个\tuyige\t0",
            "有没有\tumeiu\t0",
            "有没有\tumu\t0",
            "世界\tshi jie\t50",
        ] {
            assert!(merged.contains(expected), "少了 {expected:?}:\n{merged}");
        }
    }

    #[test]
    fn key_union_never_drops_any_distinct_key() {
        // 不变量：并集必须覆盖两侧所有不同的去重键（同键取大权重是允许的）。
        let local = "甲\tjia\t10\n乙\tyi\t3\n有个\tuig\t0\n有个\tuyig\t0\n";
        let remote = "甲\tjia\t20\n丙\tbing\t5\n有个\tuyige\t0\n";
        let merged = key_union(local, remote);
        for line in local.lines().chain(remote.lines()) {
            let key = dedupe_key(line);
            assert!(
                merged.lines().any(|row| dedupe_key(row) == key),
                "合并后丢了键 {key:?}:\n{merged}"
            );
        }
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

    #[test]
    fn two_column_phrases_keep_codes_but_opencc_keeps_one_mapping() {
        let phrase = merge_text_for_path(
            "custom_phrase.txt",
            MergeMode::KeyUnion,
            "有一个\tuig\n",
            "有一个\tuyig\n",
        )
        .unwrap();
        assert_eq!(phrase.lines().count(), 2);
        let opencc = merge_text_for_path(
            "opencc/custom.txt",
            MergeMode::KeyUnion,
            "裏\t里\n",
            "裏\t裡\n",
        )
        .unwrap();
        assert_eq!(opencc, "裏\t里\n");
    }

    #[test]
    fn dict_comments_numeric_codes_and_percent_weights_are_preserved() {
        let header = "# Rime dictionary\n# encoding: utf-8\n\n---\nname: example\n...\n";
        let merged = dict_union(
            &format!("{header}甲\t1\n甲\tjia\t10%\n"),
            &format!("{header}甲\t2\n甲\tjia\t20%\n"),
        );
        assert!(merged.starts_with(header));
        assert!(merged.contains("甲\t1\n"));
        assert!(merged.contains("甲\t2\n"));
        assert!(merged.contains("甲\tjia\t20%\n"));
        assert!(!merged.contains("甲\tjia\t10%\n"));
        assert_eq!(dict_union(&merged, &merged), merged);
    }

    #[test]
    fn dict_weight_column_is_taken_from_declared_columns() {
        for columns in [
            "columns: [text, weight, code]",
            "columns:\n  - text\n  - weight\n  - code",
        ] {
            let header = format!("---\nname: example\n{columns}\n...\n");
            let merged = dict_union(
                &format!("{header}甲\t10%\tj\n"),
                &format!("{header}甲\t20%\tj\n"),
            );
            assert!(merged.contains("甲\t20%\tj\n"));
            assert!(!merged.contains("甲\t10%\tj\n"));
        }
    }

    #[test]
    fn dictionary_rows_follow_the_retained_header_when_column_order_differs() {
        let local = "---\nname: words\ncolumns: [text, code, weight]\n...\n甲\tjia\t10\n";
        let remote = "---\nname: words\ncolumns: [text, weight, code]\n...\n甲\t20\tjia\n";
        let merged = dict_union(local, remote);
        assert!(merged.starts_with("---\nname: words\ncolumns: [text, code, weight]\n...\n"));
        assert!(merged.ends_with("甲\tjia\t20\n"));
        assert_eq!(merged.matches("甲\t").count(), 1);
    }
}
