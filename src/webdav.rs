use anyhow::{Context, Result, bail};
use percent_encoding::percent_decode_str;
use reqwest::{
    StatusCode,
    blocking::{Client, Response},
    header,
};
use roxmltree::Document;
use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime},
};
use url::Url;

#[derive(Clone)]
pub struct RemoteFile {
    pub relative: String,
    pub modified: Option<SystemTime>,
}
pub struct CleanupOutcome {
    pub removed: Vec<String>,
    pub failed: Vec<(String, String)>,
}
pub struct WebDav {
    root: Url,
    client: Client,
    username: String,
    password: String,
}

impl WebDav {
    pub fn new(url: &str, username: &str, password: &str) -> Result<Self> {
        let root =
            Url::parse(&format!("{}/", url.trim_end_matches('/'))).context("WebDAV 地址无效")?;
        let client = Client::builder().timeout(Duration::from_secs(90)).build()?;
        Ok(Self {
            root,
            client,
            username: username.trim().into(),
            password: password.into(),
        })
    }
    fn request(&self, method: &str, url: Url) -> reqwest::blocking::RequestBuilder {
        self.client
            .request(method.parse().unwrap(), url)
            .basic_auth(&self.username, Some(&self.password))
    }
    fn uri(&self, relative: &str, directory: bool) -> Result<Url> {
        let encoded = relative
            .replace('\\', "/")
            .split('/')
            .filter(|x| !x.is_empty())
            .map(|x| {
                url::form_urlencoded::byte_serialize(x.as_bytes())
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
        if response.status().is_success() || response.status().as_u16() == 207 {
            Ok(response)
        } else if response.status() == StatusCode::UNAUTHORIZED {
            bail!(
                "{action}失败: HTTP 401 Unauthorized（请核对用户名大小写和密码，并确认该账号拥有 WebDAV 目录权限）"
            )
        } else {
            bail!("{action}失败: HTTP {}", response.status())
        }
    }
    pub fn test(&self) -> Result<()> {
        let name = format!(".rime-sync-test-{}.tmp", std::process::id());
        let data = b"rime-webdav-test";
        Self::ok(
            self.request("PUT", self.uri(&name, false)?)
                .body(data.to_vec())
                .send()?,
            "上传 WebDAV 测试文件",
        )?;
        let result = (|| {
            let bytes = Self::ok(
                self.request("GET", self.uri(&name, false)?).send()?,
                "下载 WebDAV 测试文件",
            )?
            .bytes()?;
            if bytes.as_ref() != data {
                bail!("WebDAV 测试文件内容不一致");
            }
            Ok(())
        })();
        let _ = self.request("DELETE", self.uri(&name, false)?).send();
        result
    }
    pub fn list_recursive(&self) -> Result<Vec<RemoteFile>> {
        let mut out = Vec::new();
        self.walk("", &mut out)?;
        Ok(out)
    }
    fn walk(&self, relative: &str, out: &mut Vec<RemoteFile>) -> Result<()> {
        let body = r#"<?xml version="1.0"?><propfind xmlns="DAV:"><prop><resourcetype/><getlastmodified/></prop></propfind>"#;
        let response = self
            .request("PROPFIND", self.uri(relative, true)?)
            .header("Depth", "1")
            .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
            .body(body)
            .send()?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        let bytes = Self::ok(response, "列出 WebDAV")?.bytes()?;
        // Some DAV servers advertise an invalid charset. XML element names are ASCII,
        // and lossy UTF-8 decoding avoids reqwest's Content-Type charset handling.
        let text = String::from_utf8_lossy(&bytes);
        let doc = Document::parse(&text).context("解析 WebDAV XML 失败")?;
        for node in doc
            .descendants()
            .filter(|n| n.has_tag_name(("DAV:", "response")))
        {
            let href = node
                .descendants()
                .find(|n| n.has_tag_name(("DAV:", "href")))
                .and_then(|n| n.text())
                .unwrap_or("");
            let item = self.relative_from_href(href)?;
            if item.is_empty() || item.trim_end_matches('/') == relative.trim_end_matches('/') {
                continue;
            }
            let collection = node
                .descendants()
                .any(|n| n.has_tag_name(("DAV:", "collection")));
            if collection {
                self.walk(item.trim_end_matches('/'), out)?;
            } else {
                let modified = node
                    .descendants()
                    .find(|n| n.has_tag_name(("DAV:", "getlastmodified")))
                    .and_then(|n| n.text())
                    .and_then(|s| httpdate::parse_http_date(s).ok());
                out.push(RemoteFile {
                    relative: item,
                    modified,
                });
            }
        }
        Ok(())
    }
    fn relative_from_href(&self, href: &str) -> Result<String> {
        let url = self.root.join(href)?;
        let base = self.root.path();
        let path = url.path().strip_prefix(base).unwrap_or(url.path());
        Ok(percent_decode_str(path.trim_start_matches('/'))
            .decode_utf8_lossy()
            .replace('/', std::path::MAIN_SEPARATOR_STR))
    }
    pub fn download(&self, files: &[RemoteFile], root: &Path) -> Result<()> {
        for remote in files {
            self.download_file(&remote.relative, &safe_local(root, &remote.relative)?, remote.modified)?;
        }
        Ok(())
    }
    pub fn download_file(&self, relative: &str, target: &Path, modified: Option<SystemTime>) -> Result<()> {
        if let Some(p) = target.parent() {
            fs::create_dir_all(p)?;
        }
        let mut response = Self::ok(
            self.request("GET", self.uri(relative, false)?).send()?,
            "下载 WebDAV 文件",
        )?;
        let header_time = response
            .headers()
            .get(header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| httpdate::parse_http_date(s).ok());
        let mut file = fs::File::create(target)?;
        std::io::copy(&mut response, &mut file)?;
        if let Some(time) = modified.or(header_time) {
            filetime::set_file_mtime(target, filetime::FileTime::from_system_time(time))?;
        }
        Ok(())
    }
    pub fn upload_file(&self, relative: &str, source: &Path) -> Result<()> {
        if let Some((dir, _)) = relative.rsplit_once('/') {
            if !dir.is_empty() {
                self.ensure_collection(dir)?;
            }
        }
        let bytes = fs::read(source)?;
        Self::ok(
            self.request("PUT", self.uri(relative, false)?)
                .body(bytes)
                .send()?,
            "上传 WebDAV 文件",
        )?;
        Ok(())
    }
    fn list_top_level(&self) -> Result<Vec<(String, bool)>> {
        let body = r#"<?xml version="1.0"?><propfind xmlns="DAV:"><prop><resourcetype/><getlastmodified/></prop></propfind>"#;
        let response = self
            .request("PROPFIND", self.uri("", true)?)
            .header("Depth", "1")
            .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
            .body(body)
            .send()?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        let bytes = Self::ok(response, "列出 WebDAV")?.bytes()?;
        let text = String::from_utf8_lossy(&bytes);
        let doc = Document::parse(&text).context("解析 WebDAV XML 失败")?;
        let mut out = Vec::new();
        for node in doc
            .descendants()
            .filter(|n| n.has_tag_name(("DAV:", "response")))
        {
            let href = node
                .descendants()
                .find(|n| n.has_tag_name(("DAV:", "href")))
                .and_then(|n| n.text())
                .unwrap_or("");
            let item = self.relative_from_href(href)?;
            if item.is_empty() {
                continue;
            }
            let collection = node
                .descendants()
                .any(|n| n.has_tag_name(("DAV:", "collection")));
            out.push((item, collection));
        }
        Ok(out)
    }
    pub fn cleanup_except(&self, keep: &[&str]) -> Result<CleanupOutcome> {
        let mut outcome = CleanupOutcome {
            removed: Vec::new(),
            failed: Vec::new(),
        };
        for (relative, is_dir) in self.list_top_level()? {
            if keep.contains(&relative.as_str()) {
                continue;
            }
            let response = self
                .request("DELETE", self.uri(&relative, is_dir)?)
                .send();
            match response {
                Ok(response)
                    if response.status() == StatusCode::NOT_FOUND
                        || response.status().is_success()
                        || response.status().as_u16() == 207 =>
                {
                    outcome.removed.push(relative);
                }
                Ok(response) => {
                    outcome
                        .failed
                        .push((relative.clone(), format!("HTTP {}", response.status())));
                }
                Err(error) => {
                    outcome.failed.push((relative, format!("{error}")));
                }
            }
        }
        Ok(outcome)
    }
    fn ensure_collection(&self, relative: &str) -> Result<()> {
        let uri = self.uri(relative, true)?;
        let probe = self
            .request("PROPFIND", uri.clone())
            .header("Depth", "0")
            .send()?;
        if probe.status().is_success() || probe.status().as_u16() == 207 {
            return Ok(());
        }
        if probe.status() != StatusCode::NOT_FOUND {
            Self::ok(probe, "检查 WebDAV 目录")?;
        }
        Self::ok(self.request("MKCOL", uri).send()?, "创建 WebDAV 目录")?;
        Ok(())
    }
}
fn safe_local(root: &Path, relative: &str) -> Result<PathBuf> {
    let mut out = root.to_path_buf();
    for c in Path::new(relative).components() {
        match c {
            Component::Normal(x) => out.push(x),
            _ => bail!("WebDAV 返回越界路径"),
        }
    }
    Ok(out)
}
