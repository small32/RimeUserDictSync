fn main() {
    println!("cargo:rerun-if-changed=weasel.ico");
    println!("cargo:rerun-if-changed=assets/sync-gear.ico");

    // 这里必须看**目标**平台而不是宿主：交叉编译时 `cfg!(windows)` 指的是宿主，
    // 会出现"为 macOS 构建却仍然去编译 Windows 资源脚本"的情况
    // （报 "Can only compile resource file when target_env is ..."）。
    // CARGO_CFG_TARGET_OS 由 cargo 按目标平台设置。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut resource = winres::WindowsResource::new();
    // ID 1：EXE 自己的图标（资源管理器/任务栏显示的就是它，保持用户的 weasel 图标不变）。
    resource.set_icon("weasel.ico");
    // ID 101：齿轮图标，**只**给「同步词库」快捷方式用。
    // 快捷方式的 IconLocation 存 EXE 路径，索引 -101 存在 .lnk 头部的 IconIndex 字段里，
    // 所以 EXE 的图标不受影响。见 src/shortcut.rs。
    resource.set_icon_with_id("assets/sync-gear.ico", "101");

    resource.compile().expect("无法嵌入 Windows 图标资源");
}
