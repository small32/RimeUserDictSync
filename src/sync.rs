use crate::{
    config::{Settings, validate_installation_id, yaml_scalar},
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
    report.progress(15);
    copy_dir_if_present(&user_dir.join("cn_dicts"), &sync_folder.join("cn_dicts"))?;
    copy_dir_if_present(&user_dir.join("en_dicts"), &sync_folder.join("en_dicts"))?;
    copy_file_if_present(
        &user_dir.join("wanxiang-lts-zh-hans.gram"),
        &sync_folder.join("wanxiang-lts-zh-hans.gram"),
    )?;
    report.log(&format!(
        "步骤 1/7：已复制 cn_dicts、en_dicts 和 gram 文件到 Sync/{installation_id}。"
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
    report.progress(65);
    let cn = dictionary::merge_directories(&local_folder, &sync_folder, "cn_dicts")?;
    let en = dictionary::merge_directories(&local_folder, &sync_folder, "en_dicts")?;
    sync_newest(&local_folder, &sync_folder, "wanxiang-lts-zh-hans.gram")?;
    ensure_webdav_installation(&local_folder)?;
    report.log(&format!(
        "步骤 5/7：词库正文并集合并完成（cn {cn} 个，en {en} 个），相同词条保留较大权重。"
    ));
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
fn copy_dir_if_present(a: &Path, b: &Path) -> Result<()> {
    if a.is_dir() {
        copy_dir(a, b)?;
    }
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
            fs::copy(e.path(), dst)?;
        }
    }
    Ok(())
}
fn copy_file_if_present(a: &Path, b: &Path) -> Result<()> {
    if a.is_file() {
        if let Some(p) = b.parent() {
            fs::create_dir_all(p)?;
        }
        fs::copy(a, b)?;
    }
    Ok(())
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
fn sync_newest(a_root: &Path, b_root: &Path, name: &str) -> Result<()> {
    let a = a_root.join(name);
    let b = b_root.join(name);
    match (a.exists(), b.exists()) {
        (false, false) => {}
        (false, true) => {
            fs::copy(b, a)?;
        }
        (true, false) => {
            fs::copy(a, b)?;
        }
        (true, true) => {
            let at = fs::metadata(&a)?
                .modified()
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let bt = fs::metadata(&b)?
                .modified()
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if at > bt {
                fs::copy(a, b)?;
            } else if bt > at {
                fs::copy(b, a)?;
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
