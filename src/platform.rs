use anyhow::{Context, Result, bail};
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

// 只有 Linux 的探测/权限判定会碰文件系统：Windows 上是 Win32 API，
// macOS 走 config.rs 那条路径。不加 cfg 的话 Windows / macOS 的
// `clippy -D warnings` 会因「未使用导入」直接失败。
#[cfg(target_os = "linux")]
use std::fs;

/// RIME 命令的最长执行时间。超时就杀掉：进程卡住会一直占着同步锁，
/// 而后台轮次没有「停止同步」按钮，只能重启程序才能恢复。
const RIME_TIMEOUT: Duration = Duration::from_secs(300);

/// RIME 两步动作：合并 userdb 快照 / 重新部署。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RimeStage {
    Sync,
    Deploy,
}

/// 一条 RIME 命令。
///
/// 这里存**完整 argv** 而不是「一个可执行文件 + 一个固定参数」，因为 Linux 上
/// 两步是两个不同的程序：`rime_dict_manager --sync` 合并 userdb，
/// `rime_deployer --build <user> <shared>` 重新部署，两者都不像
/// `WeaselDeployer.exe /sync` 那样共用一个可执行文件。
pub struct RimeCommand {
    pub sync: Vec<String>,
    pub deploy: Vec<String>,
    /// 部署之后尽力让输入法前端重新加载（`fcitx5-remote -r`）。
    /// 失败不算同步失败：数据已经落到 `build/`，切换输入法时自然会用上。
    pub reload: Vec<String>,
    /// 子进程的工作目录。
    ///
    /// librime 的命令行工具从**当前目录**读 `installation.yaml`，并据此确定
    /// `installation_id` 与 `sync_dir`，所以必须把 cwd 固定到 RIME 用户目录；
    /// 否则它会去 `~/.config/ibus/rime` 之类的位置找，读到空配置后静默不合并。
    pub cwd: Option<PathBuf>,
}

impl RimeCommand {
    fn argv(&self, stage: RimeStage) -> &[String] {
        match stage {
            RimeStage::Sync => &self.sync,
            RimeStage::Deploy => &self.deploy,
        }
    }

    /// 供日志与报错使用的可读命令行。
    pub fn describe(&self, stage: RimeStage) -> String {
        self.argv(stage).join(" ")
    }
}

pub fn default_user_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        use winreg::{RegKey, enums::*};
        let current_user = RegKey::predef(HKEY_CURRENT_USER);
        for flags in [KEY_READ | KEY_WOW64_64KEY, KEY_READ | KEY_WOW64_32KEY] {
            if let Ok(key) = current_user.open_subkey_with_flags("SOFTWARE\\Rime\\Weasel", flags)
                && let Ok::<String, _>(dir) = key.get_value("RimeUserDir")
                && !dir.trim().is_empty()
            {
                return PathBuf::from(expand_env(&dir));
            }
        }
        return std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("Rime");
    }
    #[cfg(target_os = "macos")]
    {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("Library/Rime");
    }
    #[cfg(target_os = "linux")]
    {
        // Linux 没有统一的 RIME 用户目录：fcitx5-rime 与 ibus-rime 各用一套；
        // 而且以 root / sudo 运行时进程的 HOME 指向 /root，真正在用的词库却在
        // 自己账号的 /home/<user> 下，所以候选要横跨 /home 下所有账号。
        let candidates = linux_rime_candidates();
        if let Some(found) = linux_pick_rime_dir(&candidates) {
            return found;
        }
        return candidates
            .into_iter()
            .map(|(_, dir)| dir)
            .next()
            .unwrap_or_default();
    }
    #[allow(unreachable_code)]
    PathBuf::new()
}

/// 候选目录的优先级：0 = 当前账号自己的 home（root 运行时就是 `/root`）。
#[cfg(target_os = "linux")]
const TIER_SELF: u8 = 0;
/// 1 = 其他账号：`SUDO_USER`、`/home` 下每个账号、以及非 root 运行时的 `/root`。
#[cfg(target_os = "linux")]
const TIER_OTHER_ACCOUNT: u8 = 1;

/// 「找到了」的门槛：目录存在，且至少有 `installation.yaml` 或 `*.userdb`。
///
/// 只认 `is_dir()` 不行：装过 fcitx5 的机器上 `~/.local/share/fcitx5/rime`
/// 可能只是个空壳（1 分），把它当结果等于同步空气，也就永远轮不到 `/home` 下
/// 那套真正在用的词库。
#[cfg(target_os = "linux")]
const LINUX_RIME_USABLE_SCORE: u8 = 2;

/// Linux 上 RIME 用户目录的候选位置，带优先级分组。
///
/// 分级的意义是用户视角的「先查自己的，再查别人的」：
/// - 以 root / sudo 拉起时，先在 **root 自己的 home** 里找，找到就用，不越权去动
///   别人的词库；
/// - root 下确实没有（这是常态：词库跟着桌面账号走），才降级到 `/home` 下各账号；
/// - 普通账号运行时，自己家目录永远是 0 层，`/root` 退到 1 层兜底。
///
/// 组内仍按 `linux_rime_dir_score` 挑最像在用的那个，组间是硬边界：
/// 0 层只要达到门槛就绝不看 1 层。
///
/// 只做只读探测：别人家 700 的 home 读不到时 `is_dir()` 直接为假，不影响结果。
#[cfg(target_os = "linux")]
fn linux_rime_candidates() -> Vec<(u8, PathBuf)> {
    let current_home = std::env::var_os("HOME").map(PathBuf::from);
    let mut candidates = Vec::new();
    for (tier, home) in linux_home_roots(&current_home) {
        // XDG 变量只描述「当前进程所属用户」的布局，套到别人身上会指到错误位置。
        let is_current = current_home.as_deref() == Some(home.as_path());
        let data_home = if is_current {
            std::env::var_os("XDG_DATA_HOME").map(PathBuf::from)
        } else {
            None
        }
        .unwrap_or_else(|| home.join(".local/share"));
        let config_home = if is_current {
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        } else {
            None
        }
        .unwrap_or_else(|| home.join(".config"));

        // fcitx5-rime：XDG 数据目录下的 fcitx5/rime。
        push_unique(&mut candidates, tier, data_home.join("fcitx5/rime"));
        // ibus-rime：XDG 配置目录下的 ibus/rime。
        push_unique(&mut candidates, tier, config_home.join("ibus/rime"));
        // 少数老发行版把 fcitx5 的 RIME 数据放在 ~/.config/fcitx5/rime。
        push_unique(&mut candidates, tier, config_home.join("fcitx5/rime"));
    }
    candidates
}

/// 可能放着 RIME 用户目录的 home 根，带优先级分组、按路径去重。
#[cfg(target_os = "linux")]
fn linux_home_roots(current_home: &Option<PathBuf>) -> Vec<(u8, PathBuf)> {
    let mut roots: Vec<(u8, PathBuf)> = Vec::new();
    // 当前账号永远排第一：root 运行时这里就是 /root 自己。
    if let Some(home) = current_home {
        push_unique(&mut roots, TIER_SELF, home.clone());
    }
    // `sudo ./RimeUserDictSync`：HOME 变成 /root，但词库在真实账号下。
    if let Some(user) = std::env::var_os("SUDO_USER") {
        let name = user.to_string_lossy();
        if !name.is_empty() && name != "root" {
            push_unique(
                &mut roots,
                TIER_OTHER_ACCOUNT,
                PathBuf::from("/home").join(name.as_ref()),
            );
        }
    }
    // 其余账号：只读扫一层 /home，不递归。
    if let Ok(entries) = fs::read_dir("/home") {
        let mut others: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
            .map(|entry| entry.path())
            .collect();
        others.sort();
        for path in others {
            push_unique(&mut roots, TIER_OTHER_ACCOUNT, path);
        }
    }
    // root 直接登录时它在 0 层；普通账号运行时它只是 1 层的兜底。
    push_unique(&mut roots, TIER_OTHER_ACCOUNT, PathBuf::from("/root"));
    roots
}

#[cfg(target_os = "linux")]
fn push_unique(list: &mut Vec<(u8, PathBuf)>, tier: u8, path: PathBuf) {
    // 同一路径只保留最先出现的那次（也就是优先级最高、最早被扫到的分组）。
    if !list.iter().any(|(_, existing)| existing == &path) {
        list.push((tier, path));
    }
}

/// 从候选里挑一个最像「正在被输入法使用」的目录。
///
/// 先按分组：0 层（当前账号）里只要有一个达到 `LINUX_RIME_USABLE_SCORE`，就用它，
/// 绝不跨到 1 层；0 层全是空的或不存在时才降级到 1 层。组内比分数，
/// 同分保留先出现的，从而让 fcitx5 优先于 ibus 之类「更常见」的位置胜出。
#[cfg(target_os = "linux")]
fn linux_pick_rime_dir(candidates: &[(u8, PathBuf)]) -> Option<PathBuf> {
    let mut tiers: Vec<u8> = candidates.iter().map(|(tier, _)| *tier).collect();
    tiers.sort_unstable();
    tiers.dedup();

    // 兜底：所有分组都没达到门槛时，仍返回「相对最像样」的那个，
    // 让上层报错时能把最可能的路径摆给用户看。
    let mut best_overall: Option<(u8, PathBuf)> = None;
    for tier in tiers {
        let mut best_in_tier: Option<(u8, PathBuf)> = None;
        for (_, dir) in candidates.iter().filter(|(t, _)| *t == tier) {
            let score = linux_rime_dir_score(dir);
            let better = |best: &Option<(u8, PathBuf)>| match best {
                Some((best_score, _)) => score > *best_score,
                None => true,
            };
            if better(&best_overall) {
                best_overall = Some((score, dir.clone()));
            }
            if better(&best_in_tier) {
                best_in_tier = Some((score, dir.clone()));
            }
        }
        if let Some((score, dir)) = best_in_tier
            && score >= LINUX_RIME_USABLE_SCORE
        {
            return Some(dir);
        }
    }
    best_overall.map(|(_, dir)| dir)
}

/// 0 = 不存在；1 = 目录存在；+1 有 `installation.yaml`；+2 有 `*.userdb`。
#[cfg(target_os = "linux")]
fn linux_rime_dir_score(dir: &Path) -> u8 {
    if !dir.is_dir() {
        return 0;
    }
    let mut score = 1;
    if dir.join("installation.yaml").is_file() {
        score += 1;
    }
    if linux_has_userdb(dir) {
        score += 2;
    }
    score
}

#[cfg(target_os = "linux")]
fn linux_has_userdb(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy().ends_with(".userdb"))
}

/// 自动探测到「别人的 home」时的说明。
///
/// 以 root / sudo 运行时 `HOME` 是 `/root`，真正在用的词库在 `/home/<user>`；
/// 不说一句的话，用户会以为自己同步的是个空目录。
#[cfg(target_os = "linux")]
pub fn rime_dir_origin_hint(dir: &Path) -> Option<String> {
    let current = std::env::var_os("HOME").map(PathBuf::from)?;
    origin_hint(dir, &current)
}

#[cfg(not(target_os = "linux"))]
pub fn rime_dir_origin_hint(_dir: &Path) -> Option<String> {
    None
}

/// 探测明细：每个候选目录的优先级分组、得分与命中特征。
///
/// 「找不到词库」时用户手里只有一句「目录不存在」，而原因可能是权限、可能是
/// 前端不同、也可能是程序跑在 root 下；把扫过的位置全列出来，一次就能看出来。
#[cfg(target_os = "linux")]
pub fn rime_probe_lines() -> Vec<String> {
    linux_rime_candidates()
        .into_iter()
        .map(|(tier, dir)| {
            let mut marks = vec![if dir.is_dir() {
                "目录存在"
            } else {
                "不存在"
            }];
            if dir.join("installation.yaml").is_file() {
                marks.push("installation.yaml");
            }
            if linux_has_userdb(&dir) {
                marks.push("*.userdb");
            }
            format!(
                "  [{}] {}（{} 分：{}）",
                tier_label(tier),
                dir.display(),
                linux_rime_dir_score(&dir),
                marks.join("、")
            )
        })
        .collect()
}

/// 候选目录的优先级标签，直接进日志给用户看。
#[cfg(target_os = "linux")]
fn tier_label(tier: u8) -> &'static str {
    match tier {
        TIER_SELF => "当前账号优先",
        _ => "其他账号备选",
    }
}

#[cfg(not(target_os = "linux"))]
pub fn rime_probe_lines() -> Vec<String> {
    Vec::new()
}

/// 候选目录清单（只要路径，按优先级顺序），供目录选择器做「常见位置」快捷跳转。
///
/// 只给 Linux 的 `picker_linux` 用，因此不留其他平台的空壳：Windows / macOS
/// 上的空壳没人调用，`clippy -D warnings` 会把它当成死代码整条报错。
#[cfg(target_os = "linux")]
pub fn rime_candidate_dirs() -> Vec<PathBuf> {
    linux_rime_candidates()
        .into_iter()
        .map(|(_, dir)| dir)
        .collect()
}

/// 给「找不到 RIME 用户目录」这类错误补上候选明细，**只用于写日志**。
///
/// 界面顶栏只有一行，塞进十来行扫描结果会把那句「点这里指定目录」淹掉；
/// 而排查又确实需要这些明细，所以拆开：界面显示原句，日志里跟一段诊断。
pub fn attach_rime_diagnostics(message: &str) -> String {
    if !message.contains("找不到 RIME 用户目录") {
        return message.to_owned();
    }
    let extra = rime_dir_diagnostics();
    if extra.is_empty() {
        message.to_owned()
    } else {
        format!("{message}{extra}")
    }
}

/// 拼成一段可直接放进日志/对话框的探测报告。
pub fn rime_dir_diagnostics() -> String {
    let lines = rime_probe_lines();
    if lines.is_empty() {
        return String::new();
    }
    let mut text = String::from(
        "\n已扫描的候选目录（先查「当前账号优先」这一组，全都没命中才降级到「其他账号备选」；\
         分数越高越像是在用的那套）：",
    );
    for line in lines {
        text.push('\n');
        text.push_str(&line);
    }
    if let Some(home) = std::env::var_os("HOME") {
        text.push_str(&format!("\n当前 HOME：{}", PathBuf::from(home).display()));
    }
    if let Some(user) = std::env::var_os("SUDO_USER") {
        text.push_str(&format!("\nSUDO_USER：{}", user.to_string_lossy()));
    }
    text
}

#[cfg(target_os = "linux")]
fn origin_hint(dir: &Path, current_home: &Path) -> Option<String> {
    if current_home.as_os_str().is_empty() || dir.starts_with(current_home) {
        return None;
    }
    Some(format!(
        "已自动使用 {} 下的 RIME 用户目录（当前进程 HOME 是 {}）。\
         若要改回自己账号下的词库，请点「指定 RIME 用户词库」。",
        dir.display(),
        current_home.display()
    ))
}

/// librime 的共享数据目录（方案与预编译产物都在这里）。
#[cfg(target_os = "linux")]
fn linux_shared_data_dir() -> Option<PathBuf> {
    ["/usr/share/rime-data", "/usr/local/share/rime-data"]
        .into_iter()
        .map(PathBuf::from)
        .find(|dir| dir.is_dir())
}

/// 在 PATH 里找一个可执行文件；找不到再看常见系统目录。
#[cfg(target_os = "linux")]
pub(crate) fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let is_executable = |path: &Path| {
        path.metadata()
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    let found = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(name))
                .find(|candidate| is_executable(candidate))
        })
        .unwrap_or_default();
    found.or_else(|| {
        ["/usr/bin", "/usr/local/bin", "/bin"]
            .into_iter()
            .map(|dir| PathBuf::from(dir).join(name))
            .find(|candidate| is_executable(candidate))
    })
}

/// 把路径转成 argv 里的字符串。
fn argument(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub fn find_rime(configured: &str, app_dir: &Path, rime_dir: &Path) -> Result<RimeCommand> {
    #[cfg(not(target_os = "windows"))]
    let _ = app_dir;
    #[cfg(not(target_os = "linux"))]
    let _ = rime_dir;
    #[cfg(target_os = "windows")]
    {
        let mut candidates = Vec::new();
        if !configured.trim().is_empty() {
            candidates.push(PathBuf::from(expand_env(configured)));
        }
        candidates.push(app_dir.join("WeaselDeployer.exe"));
        use winreg::{RegKey, enums::*};
        for hive in [
            RegKey::predef(HKEY_LOCAL_MACHINE),
            RegKey::predef(HKEY_CURRENT_USER),
        ] {
            for flags in [KEY_READ | KEY_WOW64_64KEY, KEY_READ | KEY_WOW64_32KEY] {
                if let Ok(key) = hive.open_subkey_with_flags("SOFTWARE\\Rime\\Weasel", flags) {
                    if let Ok::<String, _>(dir) = key.get_value("WeaselRoot") {
                        candidates.push(PathBuf::from(expand_env(&dir)).join("WeaselDeployer.exe"));
                    }
                    if let Ok::<String, _>(dir) = key.get_value("InstallDir") {
                        add_install_dir_candidates(&mut candidates, Path::new(&expand_env(&dir)));
                    }
                }
            }
        }
        for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(dir) = std::env::var_os(variable) {
                add_install_dir_candidates(&mut candidates, &PathBuf::from(dir).join("Rime"));
            }
        }
        let executable = candidates
            .into_iter()
            .find(|p| p.is_file())
            .context("找不到 WeaselDeployer.exe，请在配置中指定路径")?;
        let executable = argument(&executable);
        Ok(RimeCommand {
            sync: vec![executable.clone(), "/sync".to_owned()],
            deploy: vec![executable, "/deploy".to_owned()],
            reload: Vec::new(),
            cwd: None,
        })
    }
    #[cfg(target_os = "macos")]
    {
        let executable = if configured.trim().is_empty() {
            PathBuf::from("/Library/Input Methods/Squirrel.app/Contents/MacOS/Squirrel")
        } else {
            PathBuf::from(expand_env(configured))
        };
        if !executable.is_file() {
            bail!("找不到鼠须管 Squirrel: {}", executable.display());
        }
        let executable = argument(&executable);
        Ok(RimeCommand {
            sync: vec![executable.clone(), "--sync".to_owned()],
            deploy: vec![executable, "--reload".to_owned()],
            reload: Vec::new(),
            cwd: None,
        })
    }
    #[cfg(target_os = "linux")]
    {
        let user_dir = if rime_dir.as_os_str().is_empty() {
            default_user_dir()
        } else {
            rime_dir.to_path_buf()
        };
        // 1) 显式配置优先：沿用旧约定，自己指定的工具收 `--sync` / `--deploy`。
        if !configured.trim().is_empty() {
            let executable = PathBuf::from(expand_env(configured));
            if !executable.is_file() {
                bail!("找不到配置指定的 RIME 工具：{}", executable.display());
            }
            let executable = argument(&executable);
            return Ok(RimeCommand {
                sync: vec![executable.clone(), "--sync".to_owned()],
                deploy: vec![executable, "--deploy".to_owned()],
                reload: Vec::new(),
                cwd: Some(user_dir),
            });
        }
        // 2) 自动探测 librime 自带的命令行工具（Debian/Ubuntu 包名 librime-bin）。
        let (Some(dict_manager), Some(deployer)) =
            (which("rime_dict_manager"), which("rime_deployer"))
        else {
            bail!(
                "未找到 librime 的命令行工具（rime_dict_manager / rime_deployer）。\n\
                 请先安装：sudo apt install librime-bin\n\
                 或在 INI 的 [rime] deployer_path 中指定可执行的部署工具。"
            );
        };
        let Some(shared) = linux_shared_data_dir() else {
            bail!(
                "找不到 RIME 共享数据目录（/usr/share/rime-data）。\n\
                 请先安装方案数据：sudo apt install librime-data"
            );
        };
        // 前端各自的重载方式：fcitx5 有 CLI，ibus 只能靠重新选择引擎，这里只做 fcitx5。
        let reload = which("fcitx5-remote")
            .map(|tool| vec![argument(&tool), "-r".to_owned()])
            .unwrap_or_default();
        Ok(RimeCommand {
            // 两个工具都从 cwd 读 installation.yaml（user_id / sync_dir）。
            sync: vec![argument(&dict_manager), "--sync".to_owned()],
            deploy: vec![
                argument(&deployer),
                "--build".to_owned(),
                argument(&user_dir),
                argument(&shared),
            ],
            reload,
            cwd: Some(user_dir),
        })
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    bail!("不支持当前平台")
}

#[cfg(target_os = "windows")]
fn add_install_dir_candidates(candidates: &mut Vec<PathBuf>, root: &Path) {
    candidates.push(root.join("WeaselDeployer.exe"));
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.filter_map(Result::ok) {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                candidates.push(entry.path().join("WeaselDeployer.exe"));
            }
        }
    }
}

pub fn run(command: &RimeCommand, stage: RimeStage, cancel: &AtomicBool) -> Result<()> {
    run_argv(command.argv(stage), command.cwd.as_deref(), cancel)?;
    // 部署完顺手让前端重新加载一次。失败只当作提示：`build/` 已经更新，
    // 用户切换输入法或重启会话时照样会生效，没必要让整轮同步失败。
    if stage == RimeStage::Deploy && !command.reload.is_empty() {
        let never = AtomicBool::new(false);
        let _ = run_argv(&command.reload, command.cwd.as_deref(), &never);
    }
    Ok(())
}

fn run_argv(argv: &[String], cwd: Option<&Path>, cancel: &AtomicBool) -> Result<()> {
    let Some((program, arguments)) = argv.split_first() else {
        bail!("RIME 命令为空");
    };
    let mut builder = Command::new(program);
    builder
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        builder.current_dir(dir);
    }
    let mut child = builder
        .spawn()
        .with_context(|| format!("无法启动 {program}"))?;
    // 单独排空 stderr：librime 的工具把真正的失败原因写在这里（比如 LevelDB 的
    // `IO error: lock ... already held by process`），只留一个退出码根本没法排查。
    // 必须开线程读，否则管道写满 64 KB 后子进程会卡住。
    let stderr_reader = child
        .stderr
        .take()
        .map(|pipe| std::thread::spawn(move || read_stderr(pipe)));
    let started = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("用户已停止同步");
        }
        if let Some(status) = child.try_wait()? {
            // 进程退出时管道仍可能有未读数据。等读线程结束后再生成提示；
            // 若后代进程继承了管道，仍允许取消并受同一个总超时约束。
            let captured = match stderr_reader {
                Some(reader) => finish_stderr(reader, cancel, started)?,
                None => StderrCapture::default(),
            };
            if status.success() {
                return Ok(());
            }
            bail!(rime_command_failure(argv, status.code(), captured));
        }
        if started.elapsed() >= RIME_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "RIME 命令超过 {} 秒仍未结束，已强制终止：{}",
                RIME_TIMEOUT.as_secs(),
                argv.join(" ")
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

#[derive(Default)]
struct StderrCapture {
    text: String,
    truncated: bool,
    lock_error: bool,
}

fn rime_command_failure(argv: &[String], code: Option<i32>, captured: StderrCapture) -> String {
    let mut details = captured.text.trim().to_owned();
    if captured.truncated {
        details.push_str("\n[stderr 已截断，仅显示前 8192 字节]");
    }
    if captured.lock_error && !is_userdb_lock_error(&details) {
        details.push_str("\n[检测到 LevelDB userdb lock 冲突]");
    }
    let mut message = format!("RIME 命令执行失败，退出码 {code:?}：{}", argv.join(" "));
    if !details.is_empty() {
        message.push_str(&format!("\n{details}"));
    }
    message.push_str(lock_hint(&details));
    message
}

fn read_stderr(mut pipe: impl Read) -> std::io::Result<StderrCapture> {
    let mut captured = StderrCapture::default();
    let mut buffer = [0u8; 4096];
    let mut tail = String::new();
    let mut has_lock = false;
    let mut has_userdb = false;
    loop {
        let read = match pipe.read(&mut buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if read == 0 {
            return Ok(captured);
        }
        let chunk = String::from_utf8_lossy(&buffer[..read]);
        let scan = format!("{tail}{chunk}").to_ascii_lowercase();
        has_lock |= scan.contains("lock");
        has_userdb |= scan.contains(".userdb") || scan.contains("leveldb");
        captured.lock_error |= is_userdb_lock_error(&scan) || (has_lock && has_userdb);
        let mut tail_start = scan.len().saturating_sub(64);
        while !scan.is_char_boundary(tail_start) {
            tail_start += 1;
        }
        tail = scan[tail_start..].to_owned();

        let mut keep = chunk.len().min(8192 - captured.text.len());
        while !chunk.is_char_boundary(keep) {
            keep -= 1;
        }
        captured.text.push_str(&chunk[..keep]);
        captured.truncated |= keep < chunk.len();
    }
}

fn finish_stderr(
    reader: std::thread::JoinHandle<std::io::Result<StderrCapture>>,
    cancel: &AtomicBool,
    started: Instant,
) -> Result<StderrCapture> {
    while !reader.is_finished() {
        if cancel.load(Ordering::Relaxed) {
            bail!("用户已停止同步");
        }
        if started.elapsed() >= RIME_TIMEOUT {
            bail!(
                "RIME 命令已退出，但读取错误输出超过 {} 秒，已停止等待",
                RIME_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("读取 RIME 错误输出的线程异常退出"))?
        .context("读取 RIME 错误输出失败")
}

/// 判断一条 stderr 是不是「userdb 被别的进程占着」。
///
/// librime 的原文有两种措辞：LevelDB 的 `IO error: lock ...LOCK: already held by
/// process` 与系统的 `Resource temporarily unavailable`（同一件事的 errno 说法）。
/// 这个判断决定要不要临时退出输入法重试，宁可窄一点，别把别的错误也当成锁冲突。
pub fn is_userdb_lock_error(details: &str) -> bool {
    let lowered = details.to_ascii_lowercase();
    lowered.contains("already held by process")
        || lowered.contains("resource temporarily unavailable")
        || (lowered.contains("lock")
            && (lowered.contains(".userdb") || lowered.contains("leveldb")))
}

/// 撞锁时界面顶栏那一句（完整 stderr 另由 [`short_error`] 之外的路径写进日志）。
///
/// 刻意**不**自动退出输入法：那会在同步瞬间让用户的输入法消失一两秒，
/// 正在打字时尤其讨厌；而 RIME 自己菜单里的「同步 / 重新部署」是在
/// fcitx5 进程内直接调 librime 的，从不撞锁。所以策略是——
/// 后台轮次照旧只搬文件（不碰 userdb，永不撞锁），
/// 真要 sync/deploy 又撞上锁时，把「点输入法菜单」这个动作交给用户。
pub const LOCK_SHORT_HINT: &str = "词库被输入法占用：跨机快照已就位，请在输入法图标右键的 RIME 菜单点「同步」，再点「重新部署」；或退出输入法后重试本程序";

/// 把一段错误压成界面能只显示一行的短句。
///
/// 顶栏只有一行高，而 RIME 命令失败时的原文带着整段 stderr（好几行）。
/// 这里只负责「界面显示什么」，日志依旧用 [`attach_rime_diagnostics`] 拿全量。
pub fn short_error(details: &str) -> String {
    if is_userdb_lock_error(details) {
        return LOCK_SHORT_HINT.to_owned();
    }
    let mut lines = details.lines().filter(|line| !line.trim().is_empty());
    let first = lines.next().unwrap_or_default().trim().to_owned();
    if lines.next().is_some() {
        first
    } else {
        details.trim().to_owned()
    }
}

/// LevelDB 单进程锁的额外提示（拼在命令 stderr 后面，只进日志/终端）。
///
/// Linux 上没有「输入法自己执行同步」这条外部通路，只能另起 `rime_dict_manager`，
/// 而正在运行的 fcitx5 / ibus 往往正占着 `*.userdb`：报错原文只有一行
/// `IO error: lock ... already held by process`，不解释的话用户完全无从下手。
///
/// 这里**只说明怎么解决，不代劳**：本程序不退出、不重启用户的输入法。
fn lock_hint(details: &str) -> &'static str {
    if !is_userdb_lock_error(details) {
        return "";
    }
    "\n提示：输入法正占用 userdb（LevelDB 只允许一个进程打开）。\n      \
     最省事的做法：在输入法图标右键的 RIME 菜单里点「同步」（必要时再点「重新部署」），\n      \
     那是在 fcitx5 进程内直接调 librime，不会撞锁。\n      \
     想让本程序自己跑完，就先退出输入法再重试：fcitx5 执行 fcitx5-remote -e；\n      \
     ibus 执行 ibus exit，同步完成后重新启动输入法即可。"
}

/// 按 Windows 命令行分隔规则给参数加引号（含空白或引号时才包裹）。
#[cfg(target_os = "windows")]
pub fn quote_windows_argument(argument: &str) -> String {
    if argument.is_empty() {
        return "\"\"".to_owned();
    }
    if !argument.contains([' ', '\t', '"']) {
        return argument.to_owned();
    }
    let mut out = String::with_capacity(argument.len() + 2);
    out.push('"');
    let mut backslashes = 0;
    for character in argument.chars() {
        match character {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                // 引号前的反斜杠要翻倍，再转义引号本身。
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push('\\');
                out.push('"');
            }
            other => {
                backslashes = 0;
                out.push(other);
            }
        }
    }
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

/// 无界面模式的结果提示。
///
/// 桌面快捷方式双击启动时没有控制台，`println!` 会石沉大海：
/// Windows 用消息框，macOS 用 osascript 原生对话框，Linux 用 libnotify 通知，
/// 其余平台退化为 stderr。
pub fn notify(title: &str, message: &str, error: bool) {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MessageBoxW,
        };
        let wide = |text: &str| {
            text.encode_utf16()
                .chain(std::iter::once(0))
                .collect::<Vec<u16>>()
        };
        let text = wide(message);
        let caption = wide(title);
        let style = MB_OK
            | MB_SETFOREGROUND
            | if error {
                MB_ICONERROR
            } else {
                MB_ICONINFORMATION
            };
        // HWND 在 windows-sys 0.52 里是 isize，0 表示无父窗口。
        unsafe { MessageBoxW(0, text.as_ptr(), caption.as_ptr(), style) };
    }
    // macOS 没有消息框 API，用 osascript 弹一个原生对话框；
    // 快捷方式（AppleScript applet）双击时没有终端，这是唯一能让用户看到结果的方式。
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display dialog \"{}\" buttons {{\"好\"}} default button 1 with title \"{}\" with icon {}",
            applescript_escape(message),
            applescript_escape(title),
            if error { "stop" } else { "note" }
        );
        let _ = Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(script)
            .status();
    }
    // Linux 用桌面通知（notify-send 来自 libnotify-bin，Mint/Ubuntu 默认自带）。
    // 之前这里只往 stderr 打印，而桌面菜单启动的进程根本没有终端，等于什么都没提示。
    #[cfg(target_os = "linux")]
    {
        let urgency = if error { "critical" } else { "normal" };
        let icon = if error {
            "dialog-error"
        } else {
            "dialog-information"
        };
        let shown = Command::new("notify-send")
            .args(["-a", "RIME 用户词库同步", "-u", urgency, "-i", icon])
            .arg(title)
            .arg(message)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !shown {
            eprintln!("{title}: {message}");
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = error;
        eprintln!("{title}: {message}");
    }
}

/// AppleScript 字符串字面量转义（反斜杠、双引号、换行）。
#[cfg(target_os = "macos")]
fn applescript_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\r', '\n'], "\\n")
}

/// 展开 %VAR% 形式的环境变量（注册表 REG_EXPAND_SZ 与旧版 INI 值可能包含）。
pub fn expand_env(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('%') {
        let after = &rest[start + 1..];
        let expanded = after.find('%').and_then(|end| {
            let name = &after[..end];
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                std::env::var(name).ok().map(|var| (end, var))
            } else {
                None
            }
        });
        out.push_str(&rest[..start]);
        if let Some((end, var)) = expanded {
            out.push_str(&var);
            rest = &after[end + 1..];
        } else {
            out.push('%');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_is_drained_past_the_display_limit_and_keeps_late_lock_errors() {
        let mut data = vec![b'x'; 12_287];
        data.extend_from_slice(b"Resource temporarily unavailable");
        let mut source = std::io::Cursor::new(data);
        let captured = read_stderr(&mut source).unwrap();
        assert_eq!(source.position(), source.get_ref().len() as u64);
        assert_eq!(captured.text.len(), 8192);
        assert!(captured.truncated);
        assert!(
            captured.lock_error,
            "跨读取块、位于截断区域的锁错误也要识别"
        );
        let message = rime_command_failure(&["rime_dict_manager".into()], Some(1), captured);
        assert!(message.contains("stderr 已截断"));
        assert!(message.contains("提示：输入法正占用 userdb"));
        assert_eq!(short_error(&message), LOCK_SHORT_HINT);
    }

    #[test]
    fn fast_failed_commands_keep_their_stderr_and_lock_hint() {
        #[cfg(target_os = "windows")]
        let argv = vec![
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()),
            "/D".into(),
            "/C".into(),
            "echo Resource temporarily unavailable 1>&2 & exit /b 7".into(),
        ];
        #[cfg(not(target_os = "windows"))]
        let argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo 'Resource temporarily unavailable' >&2; exit 7".into(),
        ];
        let error = run_argv(&argv, None, &AtomicBool::new(false))
            .unwrap_err()
            .to_string();
        assert!(error.contains("Resource temporarily unavailable"));
        assert!(error.contains("退出码 Some(7)"));
        assert_eq!(short_error(&error), LOCK_SHORT_HINT);
    }

    #[test]
    fn waiting_for_stderr_respects_cancellation_and_the_command_deadline() {
        for cancelled in [true, false] {
            let reader = std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                Ok(StderrCapture::default())
            });
            let started = if cancelled {
                Instant::now()
            } else {
                Instant::now() - RIME_TIMEOUT
            };
            let error = finish_stderr(reader, &AtomicBool::new(cancelled), started)
                .err()
                .expect("不能无限等待仍持有 stderr 管道的后代进程")
                .to_string();
            assert!(error.contains(if cancelled {
                "用户已停止"
            } else {
                "已停止等待"
            }));
        }
    }

    /// 「找不到词库」时唯一的线索就是这份明细，至少要能列出来并带上得分。
    #[cfg(target_os = "linux")]
    #[test]
    fn probe_lines_list_candidates_with_scores() {
        let lines = rime_probe_lines();
        assert!(!lines.is_empty());
        assert!(lines.iter().all(|line| line.contains("分：")));
        let text = rime_dir_diagnostics();
        assert!(text.contains("已扫描的候选目录"));
    }

    /// 界面只显示一句，明细跟去日志：这两者的分界线就在这个函数里。
    #[cfg(target_os = "linux")]
    #[test]
    fn diagnostics_are_attached_only_for_the_missing_rime_dir_error() {
        let short = "找不到 RIME 用户目录 /nope，请点击「指定 RIME 用户词库」";
        let full = attach_rime_diagnostics(short);
        assert!(full.starts_with(short), "原句必须原样保留在最前面");
        assert!(full.contains("已扫描的候选目录"), "明细要跟上");
        assert!(full.contains("[当前账号优先]"));

        // 别的错误一个字都不加，免得日志里冒出无关的扫描结果。
        for other in [
            "尚未配置 WebDAV 地址或用户名",
            "尚未完成设备设置：请先指定 RIME 用户词库并设置设备 ID 与密码。",
            "",
        ] {
            assert_eq!(attach_rime_diagnostics(other), other);
        }
    }

    /// 撞锁时不代用户退出输入法，只给一句能照做的；完整 stderr 留在日志里。
    #[test]
    fn short_error_turns_a_lock_conflict_into_one_actionable_sentence() {
        let details = "失败: RIME 命令执行失败，退出码 Some(1)：/usr/bin/rime_dict_manager --sync\n\
                       E20261022 14:50:25 level_db.cc:259] Error opening db 'rime_ice': IO error: lock \
                       /home/u/.local/share/fcitx5/rime/./rime_ice.userdb/LOCK: Resource temporarily unavailable\n\
                       E20261022 14:50:26 user_dict_manager.cc:198] failed to merge snapshot file\n\
                       提示：输入法正占用 userdb（LevelDB 只允许一个进程打开）。";
        let short = short_error(details);
        assert_eq!(short, LOCK_SHORT_HINT);
        assert!(!short.contains('\n'), "顶栏只有一行，不能带换行");
        assert!(short.contains("同步"), "必须告诉用户点哪里");
    }

    /// 其他错误：多行压成首行，单行原样返回——顶栏永远只放一行。
    #[test]
    fn short_error_collapses_multiline_details_to_the_first_line() {
        let multiline =
            "RIME 命令执行失败，退出码 Some(1)：/usr/bin/rime_deployer\nE2026 stderr 第二行";
        assert_eq!(
            short_error(multiline),
            "RIME 命令执行失败，退出码 Some(1)：/usr/bin/rime_deployer"
        );

        let single = "尚未配置 WebDAV 地址或用户名";
        assert_eq!(short_error(single), single);

        // 首行是空行时不能返回空串，否则顶栏会变成一片空白。
        assert_eq!(short_error("\n\n真正的原因"), "真正的原因");
    }

    /// 锁判定要窄：别把普通 stderr 也当成锁冲突，否则界面会给错指引。
    #[test]
    fn lock_detection_matches_leveldb_and_errno_wording_only() {
        assert!(is_userdb_lock_error(
            "lock ...LOCK: already held by process"
        ));
        assert!(is_userdb_lock_error("Resource temporarily unavailable"));
        assert!(is_userdb_lock_error(
            "Error opening db 'rime_ice': IO error: lock /x/rime_ice.userdb/LOCK"
        ));
        assert!(!is_userdb_lock_error("failed to merge snapshot file"));
        assert!(!is_userdb_lock_error("no such file or directory"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_rime_dir_score_prefers_a_directory_that_is_really_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        assert_eq!(linux_rime_dir_score(&missing), 0);

        let empty = dir.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert_eq!(linux_rime_dir_score(&empty), 1);

        let configured = dir.path().join("configured");
        fs::create_dir_all(&configured).unwrap();
        fs::write(
            configured.join("installation.yaml"),
            "installation_id: pc01\n",
        )
        .unwrap();
        assert_eq!(linux_rime_dir_score(&configured), 2);

        let used = dir.path().join("used");
        fs::create_dir_all(used.join("luna_pinyin.userdb")).unwrap();
        fs::write(used.join("installation.yaml"), "installation_id: pc01\n").unwrap();
        assert_eq!(linux_rime_dir_score(&used), 4);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_pick_skips_a_stale_root_directory_and_finds_the_real_account() {
        // 复现用户场景：以 root 运行，/root 下有个空的 fcitx5/rime（只剩 1 分），
        // 真正在用的词库在 /home/<user>（installation.yaml + userdb，4 分）。
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("root").join("fcitx5").join("rime");
        let real = dir
            .path()
            .join("home")
            .join("alice")
            .join(".local/share/fcitx5/rime");
        fs::create_dir_all(&stale).unwrap();
        fs::create_dir_all(real.join("luna_pinyin.userdb")).unwrap();
        fs::write(real.join("installation.yaml"), "installation_id: pc01\n").unwrap();

        let picked = linux_pick_rime_dir(&[(TIER_SELF, stale), (TIER_OTHER_ACCOUNT, real.clone())])
            .expect("应挑出一个目录");
        assert_eq!(picked, real);
    }

    /// 用户明确要求的行为：root 下自己有一套词库时先用 root 的，
    /// 哪怕 /home 下那套分数更高也不许越界。
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_pick_prefers_root_own_dictionary_over_a_higher_scoring_other_account() {
        let dir = tempfile::tempdir().unwrap();
        let root_own = dir.path().join("root").join(".local/share/fcitx5/rime");
        let alice = dir
            .path()
            .join("home")
            .join("alice")
            .join(".local/share/fcitx5/rime");
        fs::create_dir_all(&root_own).unwrap();
        fs::write(
            root_own.join("installation.yaml"),
            "installation_id: pc01\n",
        )
        .unwrap();
        // /home/alice 更「完整」：既有 installation.yaml 也有 userdb。
        fs::create_dir_all(alice.join("luna_pinyin.userdb")).unwrap();
        fs::write(alice.join("installation.yaml"), "installation_id: pc02\n").unwrap();
        assert!(linux_rime_dir_score(&alice) > linux_rime_dir_score(&root_own));

        let picked =
            linux_pick_rime_dir(&[(TIER_SELF, root_own.clone()), (TIER_OTHER_ACCOUNT, alice)])
                .expect("应挑出一个目录");
        assert_eq!(picked, root_own);
    }

    /// root 下连目录都没有（常态）时才降级到 /home 下的账号。
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_pick_falls_back_to_home_only_when_the_current_account_has_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let missing_root = dir.path().join("root").join(".local/share/fcitx5/rime");
        let alice = dir
            .path()
            .join("home")
            .join("alice")
            .join(".local/share/fcitx5/rime");
        fs::create_dir_all(alice.join("luna_pinyin.userdb")).unwrap();
        fs::write(alice.join("installation.yaml"), "installation_id: pc02\n").unwrap();

        let picked = linux_pick_rime_dir(&[
            (TIER_SELF, missing_root),
            (TIER_OTHER_ACCOUNT, alice.clone()),
        ])
        .expect("应挑出一个目录");
        assert_eq!(picked, alice);
    }

    /// 全都不达标时仍要给出一个路径，方便报错时把最可能的位置摆给用户。
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_pick_still_returns_the_best_effort_when_nothing_is_usable() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("root").join(".local/share/fcitx5/rime");
        fs::create_dir_all(&empty).unwrap();
        let missing = dir.path().join("home").join("alice").join("rime");
        let picked =
            linux_pick_rime_dir(&[(TIER_SELF, empty.clone()), (TIER_OTHER_ACCOUNT, missing)])
                .expect("应挑出一个目录");
        // 存在的 1 分优于不存在的 0 分，即便它来自 0 层。
        assert_eq!(picked, empty);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_pick_keeps_the_first_when_scores_tie() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first").join("rime");
        let second = dir.path().join("second").join("rime");
        for path in [&first, &second] {
            fs::create_dir_all(path).unwrap();
            fs::write(path.join("installation.yaml"), "installation_id: pc01\n").unwrap();
        }
        let picked =
            linux_pick_rime_dir(&[(TIER_SELF, first.clone()), (TIER_SELF, second)]).unwrap();
        assert_eq!(picked, first);
    }

    /// 同一个 home 被两条规则命中时（例如 HOME 就是 /root，而兜底又会加一次 /root），
    /// 只保留优先级最高的那次，避免 0 层被 1 层的重复项覆盖。
    #[cfg(target_os = "linux")]
    #[test]
    fn push_unique_keeps_the_first_tier_for_a_repeated_path() {
        let mut list: Vec<(u8, PathBuf)> = Vec::new();
        push_unique(&mut list, TIER_SELF, PathBuf::from("/root"));
        push_unique(&mut list, TIER_OTHER_ACCOUNT, PathBuf::from("/root"));
        assert_eq!(list, vec![(TIER_SELF, PathBuf::from("/root"))]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn hint_mentions_the_other_account_only_when_it_differs() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home").join("alice");
        let own = home.join(".local/share/fcitx5/rime");
        fs::create_dir_all(&own).unwrap();
        // 自己家目录下的：不提示。
        assert!(origin_hint(&own, &home).is_none());

        // 别人的：提示里要同时出现选中的目录和当前 HOME，用户才知道发生了什么。
        let other = dir
            .path()
            .join("home")
            .join("bob")
            .join(".local/share/fcitx5/rime");
        fs::create_dir_all(&other).unwrap();
        let hint = origin_hint(&other, &home).expect("跨账号应给提示");
        assert!(hint.contains(&other.display().to_string()));
        assert!(hint.contains(&home.display().to_string()));
    }

    #[test]
    fn leveldb_lock_errors_get_an_actionable_hint() {
        // librime 的原文，Linux 上几乎必然遇到（fcitx5 占着 userdb 时）。
        let stderr = "IO error: lock /home/me/.local/share/fcitx5/rime/rime.userdb/LOCK: \
                      already held by process";
        assert!(lock_hint(stderr).contains("fcitx5-remote -e"));
        assert!(lock_hint("Resource temporarily unavailable").contains("ibus exit"));
        assert_eq!(lock_hint("permission denied"), "");
        assert_eq!(lock_hint(""), "");
    }

    #[test]
    fn rime_command_keeps_sync_and_deploy_apart() {
        // Linux 上这两步是两个不同的程序，不能退化成「同一个 exe 两个参数」。
        let command = RimeCommand {
            sync: vec!["rime_dict_manager".to_owned(), "--sync".to_owned()],
            deploy: vec![
                "rime_deployer".to_owned(),
                "--build".to_owned(),
                "/home/me/rime".to_owned(),
            ],
            reload: Vec::new(),
            cwd: Some(PathBuf::from("/home/me/rime")),
        };
        assert_eq!(
            command.describe(RimeStage::Sync),
            "rime_dict_manager --sync"
        );
        assert_eq!(
            command.describe(RimeStage::Deploy),
            "rime_deployer --build /home/me/rime"
        );
    }

    #[test]
    fn expand_env_expands_known_variables_and_keeps_unknown() {
        let path = std::env::var("PATH").unwrap_or_default();
        assert!(!path.is_empty());
        assert_eq!(expand_env("x%PATH%y"), format!("x{path}y"));
        assert_eq!(
            expand_env("%NO_SUCH_VARIABLE_XYZ%"),
            "%NO_SUCH_VARIABLE_XYZ%"
        );
        assert_eq!(expand_env("100%"), "100%");
        assert_eq!(expand_env("%%"), "%%");
    }
}
