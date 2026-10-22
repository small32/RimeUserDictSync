use anyhow::{Context, Result, bail};
use percent_encoding::percent_decode_str;
use reqwest::{
    Method, StatusCode,
    blocking::{Client, RequestBuilder, Response},
    header,
};
use roxmltree::Document;
use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime},
};
use url::Url;

use crate::hash::system_time_secs;

/// 远端条目的元数据（§3：优先用 etag/size/mtime 粗筛，避免无谓下载）。
#[derive(Clone, Debug, Default)]
pub struct RemoteEntry {
    /// 相对于同步根的路径，统一使用 `/` 分隔。
    pub relative: String,
    pub is_dir: bool,
    pub etag: Option<String>,
    pub length: Option<u64>,
    pub modified: Option<SystemTime>,
}

impl RemoteEntry {
    pub fn modified_secs(&self) -> Option<i64> {
        self.modified.map(system_time_secs)
    }
}

/// PUT 的结果：`If-Match` 不匹配时为 `PreconditionFailed`（§4.4 A 档）。
pub enum PutOutcome {
    Uploaded { etag: Option<String> },
    PreconditionFailed,
}

pub enum MkcolOutcome {
    Created,
    AlreadyExists,
}

/// 服务端能力探测结果（§9），缓存进 `device.json` 以避免每轮重复探测。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Capabilities {
    pub propfind: bool,
    pub etag: bool,
    /// 支持 `If-Match` 条件写（§4.4 A 档）。
    pub if_match: bool,
    pub move_supported: bool,
    pub mkcol: bool,
    pub delete: bool,
}

impl Capabilities {
    /// 按服务端能力选择上传校验档位（§4.4）。
    pub fn tier(&self) -> UploadTier {
        if self.if_match {
            UploadTier::Conditional
        } else if self.etag {
            UploadTier::EtagOnly
        } else {
            UploadTier::MetadataOnly
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "PROPFIND {}, ETag {}, If-Match {}, MOVE {}, MKCOL {}, DELETE {}",
            yes_no(self.propfind),
            yes_no(self.etag),
            yes_no(self.if_match),
            yes_no(self.move_supported),
            yes_no(self.mkcol),
            yes_no(self.delete),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadTier {
    /// A 档：PUT 带 `If-Match`，412 就重新拉取再合并。
    Conditional,
    /// B 档：上传前重取 ETag，若已变化则本轮不传。
    EtagOnly,
    /// C 档：只有 size/mtime，必要时下载全文比 hash。
    MetadataOnly,
}

impl UploadTier {
    pub fn label(self) -> &'static str {
        match self {
            Self::Conditional => "A 档（ETag + If-Match）",
            Self::EtagOnly => "B 档（仅 ETag，上传前重取）",
            Self::MetadataOnly => "C 档（size + mtime 粗筛）",
        }
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "支持" } else { "不支持" }
}

pub struct WebDav {
    root: Url,
    client: Client,
    collection_client: Client,
    username: String,
    password: String,
}

impl WebDav {
    pub fn new(url: &str, username: &str, password: &str) -> Result<Self> {
        let root =
            Url::parse(&format!("{}/", url.trim_end_matches('/'))).context("WebDAV 地址无效")?;
        let build_client = || {
            Client::builder()
                .connect_timeout(Duration::from_secs(30))
                // 单个大词库文件在慢速网络上可能远超 90 秒。
                .timeout(Duration::from_secs(600))
                .user_agent(concat!("RimeUserDictSync/", env!("CARGO_PKG_VERSION")))
        };
        let client = build_client().build()?;
        // MKCOL 重定向不代表创建成功，也不能跟随到另一个目录执行创建。
        let collection_client = build_client()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            root,
            client,
            collection_client,
            username: username.trim().to_owned(),
            password: password.to_owned(),
        })
    }

    fn request(&self, method: &str, url: Url) -> Result<RequestBuilder> {
        let method = Method::from_bytes(method.as_bytes())
            .with_context(|| format!("非法 HTTP 方法 {method}"))?;
        Ok(self
            .client
            .request(method, url)
            .basic_auth(&self.username, Some(&self.password)))
    }

    fn uri(&self, relative: &str, directory: bool) -> Result<Url> {
        let encoded = relative
            .replace('\\', "/")
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(|segment| {
                url::form_urlencoded::byte_serialize(segment.as_bytes())
                    .collect::<String>()
                    .replace('+', "%20")
            })
            .collect::<Vec<_>>()
            .join("/");
        let suffix = if directory && !encoded.is_empty() {
            format!("{encoded}/")
        } else {
            encoded
        };
        Ok(self.root.join(&suffix)?)
    }

    fn ok(response: Response, action: &str) -> Result<Response> {
        let status = response.status();
        if status.is_success() || status.as_u16() == 207 {
            Ok(response)
        } else if status == StatusCode::UNAUTHORIZED {
            bail!(
                "{action}失败: HTTP 401 Unauthorized（请核对用户名大小写和密码，并确认该账号拥有该 WebDAV 目录的读写权限）"
            )
        } else if status == StatusCode::FORBIDDEN {
            bail!("{action}失败: HTTP 403 Forbidden（该账号没有写权限，或共享为只读）")
        } else if status == StatusCode::INSUFFICIENT_STORAGE {
            bail!("{action}失败: HTTP 507 配额不足（坚果云免费版每月 1GB 上传）")
        } else {
            bail!("{action}失败: HTTP {status}")
        }
    }

    fn propfind_body() -> &'static str {
        r#"<?xml version="1.0" encoding="utf-8"?><propfind xmlns="DAV:"><prop><resourcetype/><getetag/><getcontentlength/><getlastmodified/></prop></propfind>"#
    }

    /// 列出某个目录的直接子项（Depth: 1），不含自身。
    pub fn list(&self, relative: &str) -> Result<Vec<RemoteEntry>> {
        let response = self
            .request("PROPFIND", self.uri(relative, true)?)?
            .header("Depth", "1")
            .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
            .body(Self::propfind_body())
            .send()?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        let entries = self.parse_propfind(Self::ok(response, "列出 WebDAV 目录")?)?;
        let requested = relative.trim_matches('/');
        Ok(entries
            .into_iter()
            .filter(|entry| {
                entry.relative.trim_matches('/') != requested && !entry.relative.is_empty()
            })
            .collect())
    }

    /// 取单个条目的元数据（Depth: 0）；不存在时返回 `None`。
    pub fn stat(&self, relative: &str) -> Result<Option<RemoteEntry>> {
        let response = self
            .request("PROPFIND", self.uri(relative, false)?)?
            .header("Depth", "0")
            .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
            .body(Self::propfind_body())
            .send()?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let requested = relative.trim_matches('/').replace('\\', "/");
        let mut entries = self.parse_propfind(Self::ok(response, "查询 WebDAV 条目")?)?;
        Ok(entries
            .iter()
            .position(|entry| entry.relative.trim_matches('/') == requested)
            .map(|index| entries.remove(index))
            .or_else(|| entries.into_iter().next()))
    }

    /// 递归列出目录树（Depth: 1 + 自行递归，兼容不支持 infinity 的服务端）。
    pub fn list_tree(&self, relative: &str) -> Result<Vec<RemoteEntry>> {
        let mut out = Vec::new();
        self.walk(relative, &mut out, 0)?;
        Ok(out)
    }

    fn walk(&self, relative: &str, out: &mut Vec<RemoteEntry>, depth: usize) -> Result<()> {
        if depth > 12 {
            bail!("WebDAV 目录层级过深，已停止递归");
        }
        for entry in self.list(relative)? {
            if entry.is_dir {
                let child = entry.relative.trim_end_matches('/').to_owned();
                out.push(entry);
                self.walk(&child, out, depth + 1)?;
            } else {
                out.push(entry);
            }
        }
        Ok(())
    }

    fn parse_propfind(&self, response: Response) -> Result<Vec<RemoteEntry>> {
        let bytes = response.bytes()?;
        // 部分服务端声明了错误的 charset；XML 元素名是 ASCII，有损解码更稳。
        let text = String::from_utf8_lossy(&bytes);
        let doc = Document::parse(&text).context("解析 WebDAV XML 失败")?;
        let mut out = Vec::new();
        for node in doc
            .descendants()
            .filter(|node| node.has_tag_name(("DAV:", "response")))
        {
            let href = node
                .descendants()
                .find(|node| node.has_tag_name(("DAV:", "href")))
                .and_then(|node| node.text())
                .unwrap_or("");
            let Ok(relative) = self.relative_from_href(href) else {
                continue;
            };
            if relative.is_empty() {
                continue;
            }
            let is_dir = node
                .descendants()
                .any(|node| node.has_tag_name(("DAV:", "collection")));
            let text_of = |name: &str| {
                node.descendants()
                    .find(|node| node.has_tag_name(("DAV:", name)))
                    .and_then(|node| node.text())
            };
            let etag = text_of("getetag")
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            let length =
                text_of("getcontentlength").and_then(|value| value.trim().parse::<u64>().ok());
            let modified =
                text_of("getlastmodified").and_then(|value| httpdate::parse_http_date(value).ok());
            out.push(RemoteEntry {
                relative,
                is_dir,
                etag,
                length,
                modified,
            });
        }
        Ok(out)
    }

    fn relative_from_href(&self, href: &str) -> Result<String> {
        let url = self.root.join(href)?;
        let base = self.root.path();
        let path = url
            .path()
            .strip_prefix(base)
            .with_context(|| format!("WebDAV 返回的条目不在同步根目录下: {href}"))?;
        Ok(percent_decode_str(path.trim_start_matches('/'))
            .decode_utf8_lossy()
            .to_string())
    }

    pub fn get(
        &self,
        relative: &str,
        target: &Path,
        fallback_mtime: Option<SystemTime>,
    ) -> Result<()> {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut response = Self::ok(
            self.request("GET", self.uri(relative, false)?)?.send()?,
            "下载 WebDAV 文件",
        )?;
        let header_time = response
            .headers()
            .get(header::LAST_MODIFIED)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| httpdate::parse_http_date(value).ok());
        let temp = crate::hash::temp_sibling(target);
        // 下载失败必须把半截临时文件删掉：它落在 Others/ 里，下一轮会被
        // 当成正常文件上传，把残缺内容扩散到所有设备。
        let written = (|| -> Result<()> {
            let mut file = fs::File::create(&temp)?;
            std::io::copy(&mut response, &mut file)?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = written {
            let _ = fs::remove_file(&temp);
            return Err(error.context(format!("下载 {relative} 失败")));
        }
        if let Some(time) = fallback_mtime.or(header_time) {
            let _ = filetime::set_file_mtime(&temp, filetime::FileTime::from_system_time(time));
        }
        fs::rename(&temp, target).with_context(|| format!("写入 {} 失败", target.display()))?;
        Ok(())
    }

    pub fn get_bytes(&self, relative: &str) -> Result<Vec<u8>> {
        let response = Self::ok(
            self.request("GET", self.uri(relative, false)?)?.send()?,
            "下载 WebDAV 文件",
        )?;
        Ok(response.bytes()?.to_vec())
    }

    /// 取文件内容；404 时返回 `None`。
    pub fn try_get_bytes(&self, relative: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .try_get_bytes_with_etag(relative)?
            .map(|(bytes, _)| bytes))
    }

    /// 内容与 ETag 必须来自同一个 GET 响应，不能再用独立的 STAT 配对。
    pub fn try_get_bytes_with_etag(
        &self,
        relative: &str,
    ) -> Result<Option<(Vec<u8>, Option<String>)>> {
        let response = self.request("GET", self.uri(relative, false)?)?.send()?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = Self::ok(response, "下载 WebDAV 文件")?;
        let etag = response
            .headers()
            .get(header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Ok(Some((response.bytes()?.to_vec(), etag)))
    }

    /// 上传字节内容；`if_match` 为 `None` 时不带条件头。
    pub fn put_bytes(
        &self,
        relative: &str,
        bytes: Vec<u8>,
        if_match: Option<&str>,
    ) -> Result<PutOutcome> {
        self.put_bytes_precondition(relative, bytes, if_match, false)
    }

    /// 只允许创建不存在的文件，防止两台设备首次同步时相互覆盖。
    pub fn put_bytes_if_absent(&self, relative: &str, bytes: Vec<u8>) -> Result<PutOutcome> {
        self.put_bytes_precondition(relative, bytes, None, true)
    }

    fn put_bytes_precondition(
        &self,
        relative: &str,
        bytes: Vec<u8>,
        if_match: Option<&str>,
        if_absent: bool,
    ) -> Result<PutOutcome> {
        if let Some((dir, _)) = relative.rsplit_once('/')
            && !dir.is_empty()
        {
            self.ensure_collections(dir)?;
        }
        let length = bytes.len();
        let mut request = self
            .request("PUT", self.uri(relative, false)?)?
            .header(header::CONTENT_LENGTH, length)
            .body(bytes);
        if let Some(etag) = if_match {
            request = request.header(header::IF_MATCH, etag);
        }
        if if_absent {
            request = request.header(header::IF_NONE_MATCH, "*");
        }
        let response = request.send()?;
        if response.status() == StatusCode::PRECONDITION_FAILED {
            return Ok(PutOutcome::PreconditionFailed);
        }
        let response = Self::ok(response, "上传 WebDAV 文件")?;
        let etag = response
            .headers()
            .get(header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Ok(PutOutcome::Uploaded { etag })
    }

    pub fn delete(&self, relative: &str) -> Result<()> {
        let response = self.request("DELETE", self.uri(relative, false)?)?.send()?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        Self::ok(response, "删除 WebDAV 条目")?;
        Ok(())
    }

    /// 删除目录：部分服务端要求集合 URL 带结尾斜杠。
    pub fn delete_collection(&self, relative: &str) -> Result<()> {
        let response = self.request("DELETE", self.uri(relative, true)?)?.send()?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        if response.status().is_success() || response.status().as_u16() == 207 {
            return Ok(());
        }
        // 退化为不带斜杠的形式再试一次。
        self.delete(relative)
    }

    /// 创建目录；405/301 只有经 PROPFIND 证实目标是集合后才视为已存在。
    pub fn mkcol(&self, relative: &str) -> Result<MkcolOutcome> {
        self.mkcol_url(self.uri(relative, true)?)
    }

    fn mkcol_url(&self, url: Url) -> Result<MkcolOutcome> {
        let response = self
            .collection_client
            .request(Method::from_bytes(b"MKCOL")?, url.clone())
            .basic_auth(&self.username, Some(&self.password))
            .send()?;
        match response.status() {
            status if status.is_success() => Ok(MkcolOutcome::Created),
            StatusCode::METHOD_NOT_ALLOWED | StatusCode::MOVED_PERMANENTLY => {
                let status = response.status();
                if self.collection_exists_at(&url)? {
                    Ok(MkcolOutcome::AlreadyExists)
                } else {
                    bail!(
                        "创建 WebDAV 目录失败: HTTP {status}，目标目录不存在或不是集合；服务器可能不支持 MKCOL"
                    )
                }
            }
            StatusCode::CONFLICT => bail!("创建 WebDAV 目录失败: HTTP 409（父目录不存在）"),
            status => {
                Self::ok(response, "创建 WebDAV 目录")?;
                let _ = status;
                Ok(MkcolOutcome::Created)
            }
        }
    }

    /// 仅接受本次目标的成功 resourcetype 属性，不能拿父目录或重定向目标充当证据。
    fn collection_exists_at(&self, url: &Url) -> Result<bool> {
        let response = self
            .collection_client
            .request(Method::from_bytes(b"PROPFIND")?, url.clone())
            .basic_auth(&self.username, Some(&self.password))
            .header("Depth", "0")
            .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
            .body(Self::propfind_body())
            .send()?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        let text = Self::ok(response, "确认 WebDAV 目录是否存在")?.text()?;
        let doc = Document::parse(&text).context("解析 WebDAV XML 失败")?;
        let requested = self.relative_from_href(url.as_str())?;
        Ok(doc
            .descendants()
            .filter(|node| node.has_tag_name(("DAV:", "response")))
            .any(|node| {
                let matches_target = node
                    .children()
                    .find(|child| child.has_tag_name(("DAV:", "href")))
                    .and_then(|child| child.text())
                    .is_some_and(|href| {
                        self.root
                            .join(href)
                            .is_ok_and(|actual| actual.origin() == url.origin())
                            && self.relative_from_href(href).is_ok_and(|actual| {
                                actual.trim_end_matches('/') == requested.trim_end_matches('/')
                            })
                    });
                matches_target
                    && node
                        .children()
                        .filter(|child| child.has_tag_name(("DAV:", "propstat")))
                        .any(|propstat| {
                            let success = propstat
                                .children()
                                .find(|child| child.has_tag_name(("DAV:", "status")))
                                .and_then(|child| child.text())
                                .and_then(|status| status.split_whitespace().nth(1))
                                .and_then(|status| status.parse::<u16>().ok())
                                .is_some_and(|status| (200..300).contains(&status));
                            success
                                && propstat
                                    .children()
                                    .filter(|child| child.has_tag_name(("DAV:", "prop")))
                                    .flat_map(|prop| prop.children())
                                    .filter(|child| child.has_tag_name(("DAV:", "resourcetype")))
                                    .any(|kind| {
                                        kind.children()
                                            .any(|child| child.has_tag_name(("DAV:", "collection")))
                                    })
                        })
            }))
    }

    /// 逐级创建祖先目录：部分服务器在父目录不存在时会拒绝 MKCOL。
    fn ensure_collections(&self, relative: &str) -> Result<()> {
        let mut current = String::new();
        for segment in relative
            .replace('\\', "/")
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
        {
            if current.is_empty() {
                current = segment;
            } else {
                current.push('/');
                current.push_str(&segment);
            }
            if let MkcolOutcome::AlreadyExists = self.mkcol(&current)? {}
        }
        Ok(())
    }

    /// MOVE：`false` 表示服务端不支持，调用方退化为 PUT 后校验 hash（§9）。
    pub fn move_entry(&self, from: &str, to: &str) -> Result<bool> {
        let destination = self.uri(to, false)?;
        let response = self
            .request("MOVE", self.uri(from, false)?)?
            .header("Destination", destination.as_str())
            .header("Overwrite", "T")
            .send()?;
        match response.status() {
            status if status.is_success() => Ok(true),
            StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED => Ok(false),
            _ => {
                Self::ok(response, "移动 WebDAV 条目")?;
                Ok(true)
            }
        }
    }

    /// 测试连接必须测写（§7.1）：MKCOL + PUT + GET + DELETE，只测 PROPFIND 会被只读共享骗过去。
    pub fn test_write(&self) -> Result<()> {
        let probe_dir = format!("__rime-sync-probe-{}", crate::identity::new_path_token(12));
        let probe_file = format!("{probe_dir}/probe.txt");
        let payload = b"rime-user-dict-sync probe".to_vec();

        match self.mkcol(&probe_dir)? {
            MkcolOutcome::Created => {}
            MkcolOutcome::AlreadyExists => {
                bail!("WebDAV 测试目录已存在（{probe_dir}），请稍后重试")
            }
        }
        let cleanup = |this: &Self| {
            let _ = this.delete(&probe_file);
            // 集合目录要用 delete_collection：部分服务端要求 URL 带结尾斜杠否则删不掉。
            let _ = this.delete_collection(&probe_dir);
        };
        let result = (|| -> Result<()> {
            match self.put_bytes(&probe_file, payload.clone(), None)? {
                PutOutcome::Uploaded { .. } => {}
                PutOutcome::PreconditionFailed => bail!("WebDAV 测试上传被条件请求拒绝"),
            }
            let downloaded = self
                .try_get_bytes(&probe_file)?
                .context("WebDAV 测试文件上传后无法读取")?;
            if downloaded != payload {
                bail!("WebDAV 测试文件内容不一致，服务端可能篡改或截断了内容");
            }
            Ok(())
        })();
        cleanup(self);
        result
    }

    /// 探测服务端能力（§9），失败的能力按不支持处理而不是让整轮同步失败。
    pub fn probe_capabilities(&self) -> Capabilities {
        let mut caps = Capabilities {
            propfind: self.list("").is_ok(),
            ..Default::default()
        };
        if let Ok(entries) = self.list("")
            && entries.iter().any(|entry| entry.etag.is_some())
        {
            caps.etag = true;
        }
        if !caps.etag
            && let Ok(Some(entry)) = self.stat("")
        {
            caps.etag = entry.etag.is_some();
        }

        let probe_dir = format!("__rime-sync-caps-{}", crate::identity::new_path_token(12));
        if matches!(self.mkcol(&probe_dir), Ok(MkcolOutcome::Created)) {
            caps.mkcol = true;
            let probe_file = format!("{probe_dir}/a.txt");
            let moved = format!("{probe_dir}/b.txt");
            if let Ok(PutOutcome::Uploaded { etag }) =
                self.put_bytes(&probe_file, b"x".to_vec(), None)
                && etag.is_some()
            {
                caps.etag = true;
            }
            // 先 MOVE 再删：源文件必须先存在，否则 MOVE 恒 404，move_supported 永远是 false。
            caps.move_supported = self.move_entry(&probe_file, &moved).unwrap_or(false);
            caps.delete = if caps.move_supported {
                // MOVE 成功，源文件已改名为 b.txt，删它顺带验证 DELETE。
                self.delete(&moved).is_ok()
            } else {
                self.delete(&probe_file).is_ok()
            };
            // If-Match 用必然不存在的 ETag 探测：期望 412，返回 200 说明被忽略。
            caps.if_match = matches!(
                self.put_bytes(&probe_file, b"y".to_vec(), Some("\"does-not-exist\"")),
                Ok(PutOutcome::PreconditionFailed)
            );
            let _ = self.delete(&probe_file);
            let _ = self.delete_collection(&probe_dir);
        }
        caps
    }
}

/// 把 `/` 分隔的相对路径安全地落到本地目录（拒绝 `..` 与绝对路径）。
pub fn safe_local(root: &Path, relative: &str) -> Result<PathBuf> {
    let mut out = root.to_path_buf();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(name) => out.push(name),
            _ => bail!("WebDAV 返回越界路径: {relative}"),
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::{
        collections::BTreeMap,
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
    };

    #[derive(Clone, Debug)]
    pub struct Request {
        pub method: String,
        pub path: String,
        pub headers: BTreeMap<String, String>,
        pub body: Vec<u8>,
    }

    pub struct Reply {
        pub status: u16,
        pub body: Vec<u8>,
        pub etag: Option<String>,
    }

    impl Reply {
        pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
            Self {
                status,
                body: body.into(),
                etag: None,
            }
        }

        pub fn versioned(body: Vec<u8>) -> Self {
            let etag = format!("\"{}\"", crate::hash::sha256_hex(&body));
            Self {
                status: 200,
                body,
                etag: Some(etag),
            }
        }
    }

    pub struct Server {
        pub url: String,
        pub requests: Arc<Mutex<Vec<Request>>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ =
                TcpStream::connect(self.url.trim_start_matches("http://").trim_end_matches('/'));
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }

    pub fn serve(mut handler: impl FnMut(&Request) -> Reply + Send + 'static) -> Server {
        let mut collections = std::collections::BTreeSet::new();
        serve_raw(move |request| {
            let collection = format!("{}/", request.path.trim_end_matches('/'));
            if request.method == "MKCOL" {
                return Reply::new(
                    if collections.insert(collection) {
                        201
                    } else {
                        405
                    },
                    Vec::new(),
                );
            }
            if request.method == "PROPFIND"
                && request
                    .headers
                    .get("depth")
                    .is_some_and(|depth| depth == "0")
                && collections.contains(&collection)
            {
                return Reply::new(207, format!("<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>{collection}</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>").into_bytes());
            }
            if request.method == "DELETE" {
                collections.remove(&collection);
            }
            handler(request)
        })
    }

    pub fn serve_raw(mut handler: impl FnMut(&Request) -> Reply + Send + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let thread = thread::spawn(move || {
            for socket in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                let mut socket = socket.unwrap();
                let mut raw = Vec::new();
                let mut buffer = [0; 4096];
                let header_end = loop {
                    let count = socket.read(&mut buffer).unwrap();
                    if count == 0 {
                        return;
                    }
                    raw.extend_from_slice(&buffer[..count]);
                    if let Some(pos) = raw.windows(4).position(|value| value == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let text = String::from_utf8_lossy(&raw[..header_end]);
                let mut lines = text.lines();
                let mut first = lines.next().unwrap().split_whitespace();
                let method = first.next().unwrap().to_owned();
                let path = first.next().unwrap().to_owned();
                let headers: BTreeMap<String, String> = lines
                    .filter_map(|line| line.split_once(':'))
                    .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
                    .collect();
                let size = headers
                    .get("content-length")
                    .map(|size| size.parse::<usize>().unwrap())
                    .unwrap_or(0);
                while raw.len() < header_end + size {
                    let count = socket.read(&mut buffer).unwrap();
                    assert_ne!(count, 0);
                    raw.extend_from_slice(&buffer[..count]);
                }
                let request = Request {
                    method,
                    path,
                    headers,
                    body: raw[header_end..header_end + size].to_vec(),
                };
                recorded.lock().unwrap().push(request.clone());
                let reply = handler(&request);
                write!(
                    socket,
                    "HTTP/1.1 {} Test\r\nContent-Length: {}\r\nConnection: close\r\n",
                    reply.status,
                    reply.body.len()
                )
                .unwrap();
                if let Some(etag) = reply.etag {
                    write!(socket, "ETag: {etag}\r\n").unwrap();
                }
                socket.write_all(b"\r\n").unwrap();
                socket.write_all(&reply.body).unwrap();
            }
        });
        Server {
            url,
            requests,
            stop,
            thread: Some(thread),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_local_rejects_traversal() {
        let root = Path::new("/tmp/root");
        assert!(safe_local(root, "../evil").is_err());
        assert!(safe_local(root, "/abs").is_err());
        assert_eq!(
            safe_local(root, "Others/a.txt").unwrap(),
            root.join("Others/a.txt")
        );
    }

    #[test]
    fn tier_selection_follows_server_capabilities() {
        let mut caps = Capabilities {
            propfind: true,
            mkcol: true,
            ..Default::default()
        };
        assert_eq!(caps.tier(), UploadTier::MetadataOnly);
        caps.etag = true;
        assert_eq!(caps.tier(), UploadTier::EtagOnly);
        caps.if_match = true;
        assert_eq!(caps.tier(), UploadTier::Conditional);
    }

    fn collection_xml(href: &str, collection: bool, status: u16) -> Vec<u8> {
        let kind = if collection { "<d:collection/>" } else { "" };
        format!("<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>{href}</d:href><d:propstat><d:prop><d:resourcetype>{kind}</d:resourcetype></d:prop><d:status>HTTP/1.1 {status} Test</d:status></d:propstat></d:response></d:multistatus>").into_bytes()
    }

    #[test]
    fn mkcol_requires_evidence_of_the_requested_collection() {
        use super::test_support::{Reply, serve_raw};
        for status in [405, 301] {
            for (href, collection, prop_status, accepted) in [
                ("/existing/", true, 200, true),
                ("/parent/", true, 200, false),
                ("/existing/", false, 200, false),
                ("/existing/", true, 404, false),
                ("http://example.invalid/existing/", true, 200, false),
            ] {
                let server = serve_raw(move |request| match request.method.as_str() {
                    "MKCOL" => Reply::new(status, Vec::new()),
                    "PROPFIND" => Reply::new(207, collection_xml(href, collection, prop_status)),
                    _ => Reply::new(500, Vec::new()),
                });
                let dav = WebDav::new(&server.url, "user", "pass").unwrap();
                assert_eq!(
                    matches!(dav.mkcol("existing"), Ok(MkcolOutcome::AlreadyExists)),
                    accepted,
                    "status={status}, href={href}, collection={collection}, property status={prop_status}"
                );
                assert!(
                    server
                        .requests
                        .lock()
                        .unwrap()
                        .iter()
                        .all(|request| matches!(request.method.as_str(), "MKCOL" | "PROPFIND"))
                );
            }
        }
    }

    #[test]
    fn unsupported_mkcol_is_not_reported_as_a_busy_device_or_supported_capability() {
        use super::test_support::{Reply, serve_raw};
        let server = serve_raw(|request| {
            Reply::new(
                if request.method == "MKCOL" { 405 } else { 404 },
                Vec::new(),
            )
        });
        let dav = WebDav::new(&server.url, "user", "pass").unwrap();
        assert!(
            dav.mkcol("pc01")
                .err()
                .unwrap()
                .to_string()
                .contains("不支持 MKCOL")
        );
        assert!(!dav.probe_capabilities().mkcol);
        let seed = crate::device::DeviceSeed::from_device(
            &crate::context::DeviceFile::default(),
            String::new(),
        );
        let error = match crate::device::claim(&dav, "pc01", None, &seed) {
            Err(error) => error,
            Ok(_) => panic!("不支持 MKCOL 时不应声称注册成功或 ID 被占用"),
        };
        assert!(error.to_string().contains("不支持 MKCOL"));
        assert!(
            server
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| matches!(request.method.as_str(), "MKCOL" | "PROPFIND"))
        );
    }

    #[test]
    fn capability_probe_never_writes_to_an_existing_probe_directory() {
        use super::test_support::{Reply, serve_raw};
        let server = serve_raw(|request| match request.method.as_str() {
            "MKCOL" => Reply::new(405, Vec::new()),
            "PROPFIND" => Reply::new(207, collection_xml(&request.path, true, 200)),
            _ => Reply::new(500, Vec::new()),
        });
        let dav = WebDav::new(&server.url, "user", "pass").unwrap();
        assert!(!dav.probe_capabilities().mkcol);
        assert!(
            server
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| matches!(request.method.as_str(), "MKCOL" | "PROPFIND"))
        );
    }

    #[test]
    fn conditional_creation_cannot_overwrite_an_existing_file() {
        use super::test_support::{Reply, serve};
        let mut exists = false;
        let server = serve(move |request| {
            if request.method == "PUT" {
                assert_eq!(
                    request.headers.get("if-none-match").map(String::as_str),
                    Some("*")
                );
                let status = if exists { 412 } else { 201 };
                exists = true;
                Reply::new(status, Vec::new())
            } else {
                Reply::new(404, Vec::new())
            }
        });
        let dav = WebDav::new(&server.url, "user", "pass").unwrap();
        assert!(matches!(
            dav.put_bytes_if_absent("Others/a.txt", b"first".to_vec())
                .unwrap(),
            PutOutcome::Uploaded { .. }
        ));
        assert!(matches!(
            dav.put_bytes_if_absent("Others/a.txt", b"second".to_vec())
                .unwrap(),
            PutOutcome::PreconditionFailed
        ));
    }
}
