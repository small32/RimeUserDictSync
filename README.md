# RIME 用户词库同步工具

跨平台 RIME（Windows 小狼毫 / macOS 鼠须管）多设备词库同步工具，Rust 实现。
设计目标：**多端最终一致、同步过程不打断输入、删除基本可用**。

核心思路是把**传输**与**生效**解耦：

- **后台纯搬运**（默认每 10 分钟）只做文件比对与搬运，不读写 `RIME/` 根目录下的源文件，
  也不调用 RIME 的 `/sync`、`/deploy`，所以对正在打字的输入法零干扰；
- **手动「同步词库」** 才执行合并、`/sync`、`/deploy`，让远端词条真正生效。

## 主要功能

- **数据分层**：`Sync/<ID>/` 放各设备的 userdb 快照（整文件增量），
  `Sync/Others/` 放文本词典的公共合并结果（词条级并集）。
- **后台搬运**：比对远端 `Others/` 与各设备 `<ID>/.index.json`，按需增量下载/上传。
  可由 **Windows 服务**常驻执行（开机即跑、不必打开本程序），未安装服务时由程序内建线程兜底。
- **手动同步**：严格按 拉取 → 并集 → 写回 → 上传 → `/sync` → `/deploy` 执行。
- **按类型合并**：`*.dict.yaml` 词条并集取大权重；短语文本按「文本 + 编码」并集（权重可省略），`opencc/*.txt` 按首列 key 并集；
  `*.yaml`（非 dict）/`*.json`/`*.lua` 走 newest。
- **三向判断**：newest 型文件双端同时修改时保留冲突副本并提示，绝不静默覆盖。
- **设备 ID / 密码**：PBKDF2 派生串、原子抢占、密码验证接管、强制接管、注销、导出/导入。
- **`session_token` 每轮校验**：同一 ID 被两台设备使用时，旧设备下一轮自动停止同步。
- **WebDAV 能力探测**：自动选择 `If-Match` / ETag / size+mtime 三档上传校验。
  探测结果会保存并绑定当前连接；更换地址或凭据后重新探测，避免沿用旧服务器的能力。
- 提供后台开关与间隔、进度条、实时日志、停止同步、连接与写入测试。
- 启动后自动检查 GitHub 最新 Release；同一套源码可在 Windows、macOS 原生编译。

## 运行环境

- Windows 10/11 并安装小狼毫；或 macOS 并安装鼠须管。
- WebDAV 服务需要支持 `PROPFIND`、`MKCOL`、`GET`、`PUT`、`DELETE`；
  `If-Match`、`MOVE` 为可选（不支持会自动降级，方案仍可用）。
- 程序所在目录（或 macOS 配置目录）必须可读写。建议不要放在 `Program Files` 下。

## 目录布局

```text
<RIME 用户目录>/                     Windows: %APPDATA%\Rime   macOS: ~/Library/Rime
├─ installation.yaml                 installation_id / sync_dir / Password(派生串)
├─ *.userdb/                         leveldb，设备私有，绝不上传
├─ my_words.dict.yaml                本地源（权威副本）
├─ .rime-sync/                       工具私有，严禁进 Sync/
│  ├─ device.json                    ★ 设备配置的权威副本
│  ├─ sync-index.json                三向判断基线与远端观测结果
│  ├─ sync-list.json                 本设备同步列表 + 排除集
│  ├─ sync.lock                      本地互斥锁
│  └─ wake                           「立刻搬运一轮」的标记，服务跑完即删
└─ Sync/                             ★ 唯一与 WebDAV 同步的目录
   ├─ <ID_A>/                        A 的快照 + .index.json + owner.json + 独立会话心跳
   ├─ <ID_B>/                        B 的快照（本机下载，供 /sync 合并）
   └─ Others/                        ★ 单一共享目录
      ├─ custom_phrase.txt
      └─ cn_dicts/base.dict.yaml
```

三处不能动的红线：

| 规则 | 原因 |
| --- | --- |
| 只把 `<RIME 用户目录>/Sync/` 纳入外部同步工具 | 整个 `Rime/` 会带上 `*.userdb/` leveldb 目录，必损坏 |
| `device.json` / `sync-index.json` / `sync-list.json` 放 `Sync/` 外 | 每台设备基线不同，被同步会互相污染 |
| `Others/` 内禁止 `*.userdb.txt` | librime 会把 `Others/` 当成一台"设备"参与合并 |
| 同步文件名一律 ASCII | macOS 中文名是 NFD 分解、Windows 是 NFC，跨端不一致 |

程序目录（Windows 为 EXE 所在目录，macOS 默认为
`~/Library/Application Support/RimeUserDictSync`）只放程序自己的东西：

```text
RimeUserDictSync.ini    连接信息与后台间隔
RimeSync.log            运行日志
```

## 首次配置

### 1. 指定 RIME 用户词库

点击「指定 RIME 用户词库」，选择包含 `installation.yaml` 的目录：

```text
Windows: %APPDATA%\Rime
macOS:   ~/Library/Rime
```

程序会读取其中的 `installation_id`，并把 `sync_dir` 写成该目录下的 `Sync`。

### 2. WebDAV 设置

填写地址、用户名、密码，点击「测试连接」。测试会执行
`MKCOL` + `PUT` + `GET` + `DELETE`，**只测 PROPFIND 会被只读共享骗过去**，
所以必须测写；同时会探测服务端能力并显示上传校验档位。

请为本程序使用专用的远端文件夹，例如 `https://dav.example.com/rime-sync/`。

### 3. 设备设置（必做）

首次使用必须在这里确定设备身份：

- **注册 / 验证并接管**：填设备 ID 与密码，点按钮。
  - 远端没有这个 ID → 用 `MKCOL` **原子抢占**，写入 `<ID>/owner.json`；
  - 远端已有这个 ID → **不会自动占用**，程序展示该 ID 的
    主机名 / 首次注册时间 / 最后同步时间 / 快照文件数；
    密码正确则接管（保留远端 userdb 快照、备份本地 `Others/`、签发新 token）；
    密码错误直接拒绝，只能换 ID。
  - 若该 ID 在 24 小时内还同步过，会额外提示"该设备似乎仍在使用"。
- **强制接管**：忘记密码时的兜底，要求重设新密码。
- **修改密码**：验证旧密码后重新派生；**不轮换 token**（本机身份没变）。
- **注销此设备**：删除远端 `<ID>/` 与本地 `Sync/<ID>/`，避免孤儿目录被永久合并。
- **导出 / 导入配置**：导出内容不含密码，重装后导入即可零冲突继续用。

设备 ID 规则：`^[A-Za-z0-9][A-Za-z0-9_-]{2,31}$`，并额外禁止 Windows 保留设备名与
`Others`/`WebDAV` 等保留目录名；查重大小写不敏感。

接管和导入后的首轮会恢复远端快照，不会用本地空目录清空远端清单。
恢复副本暂存于 `Sync/__rime_restore_<ID>/`，供 RIME 合并，成功后清理。
设备设置通过远端 `__rime-owner-lock-<id>/` 互斥；若设置进程异常退出留下锁，
确认所有设备均未执行设备设置后，可手动删除提示中的锁目录再重试。

> **密码不是安全边界，是防误操作。** WebDAV 是纯存储、没有服务端鉴权，
> 任何持有 WebDAV 凭据的人都能拿到 `owner.json`。密码真正挡住的是你自己在另一台
> 机器上误填同一个 ID。
>
> `installation.yaml` 的 `Password` 保存的是 **KDF 派生串而不是明文**，
> 因为它可能在其它同步工具的范围内、且用户经常直接查看或贴出来求助。
> WebDAV 自身密码建议放进系统钥匙串，并**不要与 ID 密码相同**。

### 4. 同步文件选择（可选）

点击「同步文件选择」，添加你**真正改过的**文件。合并方式按类型自动判定，不需要手选：

| 类型 | 例子 | 规则 |
| --- | --- | --- |
| `*.dict.yaml` | `my_words.dict.yaml`、`cn_dicts/base.dict.yaml` | 词条级并集，同 key 取大权重 |
| `*.txt` | `custom_phrase.txt`、`opencc/emoji.txt` | 按首列 key 并集 |
| `*.yaml`（非 dict）、`*.json`、`*.lua` | `default.custom.yaml`、`opencc/t2s.json` | newest（带三向判断） |
| `*.userdb.txt` | — | **禁止加入**（界面会拦截） |

`Others/` 里的文件分两种情况：

| 情况 | 处理 |
| --- | --- |
| 本设备已登记 | 参与并集 → 写回本地源 → 覆盖 `Others/` |
| 本设备未登记（他机登记过） | **只搬运，不合并不写回本地** |

建议把自定义内容外置：新建 `my_words.dict.yaml` 挂到 schema 的 `dictionary:` 列表，
而不是直接改 `cn_dicts/base.dict.yaml`。这样方案升级不会覆盖你的改动，也不存在版本漂移。

**不要同步**：没改过的方案原版文件（避免版本漂移）、`*.userdb/`（leveldb 目录）、
`build/`、`installation.yaml`、`user.yaml`、万象 `.gram` 与模型文件。

## 使用方法

### 后台搬运

主窗口勾选「后台搬运」并设置间隔（默认 10 分钟）。一轮的动作是：

```text
取本地锁 → 校验 session_token
① 拉：远端 Others/ 有变化就下载覆盖本地
② 拉：比对其他设备 <ID_x>/.index.json，增量下载变化的 userdb 快照
③ 推：本地 Others/ 有变化则上传
④ 推：本设备 <ID_A>/ 快照有变化则上传 + 更新 .index.json
解锁
```

这一轮**完全不读、不写 `RIME/` 根目录下的源文件**，所以打字不受影响。
比对优先用 PROPFIND 的 `getetag`，拿不到就用 `size`+`mtime`，两者都不可信才下载全文比 hash。

### 手动「同步词库」

```
取本地锁 → 先跑完一轮后台搬运（保证拉到最新）
→ ① 合并：Rime/<自选文件> ∪ Others/<同名文件>  → 写回 Rime/<自选文件>
→ ② 上传 Others/ 的变更（不等下一个 10 分钟）
→ ③ RIME /sync：合并 Sync/<各ID>/ 下的 userdb 快照（自造词与删除传播在此）
→ ④ 上传本次 /sync 产生的本机快照
→ ⑤ /deploy（macOS 为 --reload）：生效
解锁
```

**顺序不能颠倒**：反过来会用本地旧内容盖掉他机已上传的新内容。

`/deploy` 会让输入法短暂不可用，正在输入的 composition 会丢，所以只在手动同步时执行。

## 后台运行方式

后台搬运有三种运行方式，三者共用同一把锁，永远不会同时动手：

| 方式 | 平台 | 何时用 | 说明 |
| --- | --- | --- | --- |
| **Windows 服务**（推荐） | Windows | 想让它一直生效 | 开机即跑，不用打开本程序；只做搬运 |
| **launchd LaunchAgent**（轻量） | macOS | 同上 | 登录后由 launchd 每 N 分钟拉起一次 `--scheduled-round`，跑完即退，**没有常驻进程** |
| 程序内建线程（兜底） | 两端 | 不想装服务/代理 | 只在程序运行期间跑；检测到服务或代理已加载就自动让位 |

### 为什么服务只做搬运

`/sync` 与 `/deploy` **必须在用户的登录会话里**操作输入法进程，而服务跑在会话 0，做不了。
搬运只是文件与 WebDAV 的读写，跑在会话 0 完全没问题。所以：

- **服务** = 后台搬运（零打扰）；
- **GUI 的「同步词库」按钮** = 合并 + `/sync` + `/deploy`（会短暂打断输入法）。

### 安装与卸载

在 GUI 底部点「安装 / 更新后台服务」，会弹出 UAC 提权提示；也可以手动执行：

```powershell
# 需要管理员权限的命令行
RimeUserDictSync.exe --install-service --app-dir "<本程序的配置目录>"
RimeUserDictSync.exe --uninstall-service
RimeUserDictSync.exe --service-status
```

| 项 | 值 |
| --- | --- |
| 服务名 | `RimeUserDictSync` |
| 显示名 | RIME 用户词库同步后台搬运 |
| 运行账号 | `LocalSystem`（不保存用户密码，改密码也不会让服务起不来） |
| 启动方式 | 自动（**延迟启动**，不拖慢开机） |
| 失败恢复 | 60 秒后重启，仍失败则 300 秒后重启，24 小时无故障后重置计数 |
| 程序位置 | `%ProgramData%\RimeUserDictSyncService\RimeUserDictSync.exe`（安装时自动复制） |
| 调度 | 每 30 秒醒一次判断是否到期；到期或收到唤醒标记就跑一轮 |
| 配置来源 | 安装时用 `--app-dir` 记下的那个目录里的同一个 INI 与同一份 `device.json` |

**服务是怎么找到配置目录的（这点踩过坑）**：一开始把它写进服务的
`lpBinaryPathName`（`sc qc` 里能看到），结果服务日志显示 `ServiceMain` **只收到服务名**，
`--app-dir` 根本没传进去 —— SCM 只把 `StartService` 的参数交给 `ServiceMain`，
命令行里的参数不会出现。所以现在按下面的顺序解析：

1. 启动参数（用 `StartService` 参数启动时有效）；
2. `HKLM\SOFTWARE\RimeUserDictSync\AppDir`（安装时写入，开机自启走的就是这条）；
3. `%ProgramData%\RimeUserDictSyncService\app-dir.txt`（安装时写入，注册表读不到时的兜底）；
4. 都没有 → 退回服务自身目录，并写一条明确的告警。

几个实际影响：

- **服务从固定位置启动**，所以你把便携版程序挪走、删掉，都不会把服务弄坏。
- **复制到 ProgramData** 意味着升级程序后要重新点一次「安装 / 更新后台服务」，
  让它把新版本复制过去（按钮对已安装的服务是原地改配置，不会报错）。
- 安装前会在当前用户会话里固定 RIME 的绝对路径；服务目录只允许管理员和 SYSTEM 写入。
  更新旧版服务时改用新的受保护目录，旧目录不再作为服务启动位置；预先存在且权限不安全的新目录会被拒绝。
- macOS 切换配置目录会同时更新内建后台线程和已安装代理；Windows 换了 Rime 用户目录后，重新点一次安装按钮即可改指向。
- 服务以 `LocalSystem` 写入 `Rime/.rime-sync/` 与 `Sync/` 的文件，属主是 `SYSTEM`；
  正常情况下 ACL 继承会让用户仍可读写，若真遇到权限问题请在 issue 里反馈。

### macOS：Rime词库同步（LaunchAgent + 菜单栏图标）

macOS 没有 SCM 服务，用两个用户级 LaunchAgent，都只在**登录后**运行：

| 组件 | Label | 形态 | 作用 |
| --- | --- | --- | --- |
| 后台搬运 | `...agent` | `StartInterval` 到点拉起 `--scheduled-round`，跑完即退 | 按间隔搬 `Sync/` 与 WebDAV |
| 菜单栏图标 | `...menubar` | 常驻 `RimeMenuBar`（`LSUIElement`，不占 Dock） | 点一下 = 同步并部署 |

```sh
"/Applications/RimeUserDictSync.app/Contents/MacOS/RimeUserDictSync" \
    --install-agent --app-dir "$HOME/Documents/RimeUserDictSync"
"/Applications/RimeUserDictSync.app/Contents/MacOS/RimeUserDictSync" --uninstall-agent
```

菜单栏图标的行为：

- 图标就是 Windows 快捷方式那个齿轮（`macos/sync-gear-template.png`，模板图，随明暗主题着色）；
- **单击鼠标左键**：依次执行 `Squirrel --sync`、`Squirrel --reload`，等价于输入法菜单里的
  「同步用户数据」+「重新部署」，只是合成一次点击；**不碰 WebDAV，也不做并集合并**；
  点击后图标下方弹出进度浮层：`正在同步用户数据…` → `正在重新部署…` → `已触发同步与部署`；
- **右键**：同步并部署 / 打开 RimeUserDictSync / 隐藏图标；
- 图标显隐跟随 `[background] enabled`：关掉后台活动后 5 秒内图标消失、助手正常退出；
  重新开启时 GUI 会 `kickstart` 它；
- plist 用 `KeepAlive = { SuccessfulExit: false }`：正常退出不重启，崩溃仍会被 launchd 拉起。

| 项 | 值 |
| --- | --- |
| 定时搬运 Label | `com.small32.rime-user-dict-sync.agent` |
| 菜单栏 Label | `com.small32.rime-user-dict-sync.menubar` |
| plist | `~/Library/LaunchAgents/` 下对应的两个 `.plist` |
| 调度 | 搬运：`StartInterval` = 间隔（分钟 × 60）；菜单栏：`RunAtLoad` + `KeepAlive` |
| 会话 | `Aqua`：登录后由 launchd 加载，登录前不跑 |
| 关掉后台活动 | `--scheduled-round` 跳过；菜单栏助手移除图标并退出 |
| 改间隔 | GUI 点「保存间隔」会重写搬运 plist 并重新 bootstrap（内容没变则不动） |

与 Windows 服务的差异：LaunchAgent 只在**用户登录后**运行，做不到登录前同步；
好处是不需要管理员权限。GUI 每 30 秒查一次 launchd，
定时搬运已加载时内建线程自动让位，不会两边同时搬。

### 调度细节

不是「睡满 10 分钟」，而是**每 30 秒醒一次，判断「下次到期时刻」到了没有**：

- 合盖休眠、待机唤醒后会**立刻补跑一轮**，不会把错过的间隔一笔勾销；
- 轮次本身的耗时不会累积成周期漂移（跑一轮 30 秒，不会变成 10 分 30 秒一次）；
- 点「立刻搬运一轮」（或保存 WebDAV / 同步列表设置）会在 `Rime/.rime-sync/` 落一个 `wake`
  标记，服务下一个 tick 就看到并立刻跑，**不用等满间隔**；
- 停止服务时会把取消标志传给正在跑的那一轮，它会在下一个检查点退出。

服务与内建线程都从磁盘重新读取设置，所以改完间隔、改完 WebDAV 地址**不需要重启服务**。

### 快捷方式「同步词库」

服务只搬运文件，**`/sync` 与 `/deploy` 必须由你在登录会话里触发**，所以才需要这个入口。
它建在**程序所在目录**（不往桌面塞东西），需要的话自己拖到桌面、开始菜单、任务栏或 Dock。
在 GUI 底部点「创建 / 更新快捷方式」，或手动执行：

```powershell
RimeUserDictSync.exe --install-shortcut --app-dir "<本程序的配置目录>"
RimeUserDictSync.exe --uninstall-shortcut
```

```sh
"/Applications/RimeUserDictSync.app/Contents/MacOS/RimeUserDictSync" \
    --install-shortcut --app-dir "$HOME/Documents/RimeUserDictSync"
```

Windows 下命令**只在该目录下生成一个文件**：`同步词库.lnk`，**不会**在磁盘上多出一个 `.ico`，
EXE 自己的图标（weasel）也保持不变。

macOS 下生成的是 `同步词库.app`（AppleScript applet）：

- 双击**不弹终端窗口**，也不占 Dock（`LSUIElement`），跑完弹一个原生对话框告知结果；
- 图标取自主程序的 `AppIcon.icns`，不是通用的脚本图标；
- applet 里执行的是主程序的 `--sync-now --notify --app-dir <配置目录>`，
  也就是**完整一轮**（拉取 → 合并写回 → 上传 → `/sync` → `/reload`）；
- 这一点与 Windows 快捷方式**故意不一致**：Windows 那边只做 `/sync` + `/deploy`。
  原因是 macOS 的 `--scheduled-round` 同样只搬运文件，若不在这里写回，
  `Sync/Others/` 就永远到不了 `RIME/`。想让两端一致，把 `shortcut_macos.rs`
  里的 `--sync-now` 换成 `--rime-sync` 即可；
- 生成时先在 `$TMPDIR` 里 `osacompile` 并 ad-hoc 签名，再整体 `rename` 到目标目录。
  直接在 `~/Documents` 这类受 FileProvider / 受管目录里签名会报
  `replacing existing signature / Operation not permitted`（`-4960`）；
- 删除时同样先 `rename` 出目标目录再清理，避免删包内可执行文件被拦。

图标信息在 `.lnk` 里分两处存放，缺一不可：

| 位置 | 内容 | 说明 |
| --- | --- | --- |
| `ShellLinkHeader.IconIndex`（偏移 56） | `-101` | 负数表示"按资源 ID 取图标" |
| StringData 的 `ICON_LOCATION` | EXE 的**裸路径** | **不能**接 `,索引` |

齿轮以资源 ID 101 内嵌在 EXE 里（`assets/sync-gear.ico`，由 `build.rs` 用 winres 嵌入），
索引 0 仍是 weasel。

> 这两条都是踩过坑才确认的，用十六进制对过 Windows 自己写出的 `.lnk`：
>
> 1. `ICON_LOCATION` **只存路径**。写成 `路径,-101` 时资源管理器会把整个字符串当成文件名，
>    弹出「Windows 找不到文件 xxxxxx.exe,-101」，图标自然为空 —— 之前那几版就是这么坏掉的。
> 2. 图标**索引**在头部的 `IconIndex` 字段里。`mslnk` 不写这个字段、也没有接口设置它，
>    所以生成后由我们补写那 4 个字节。
>
> 齿轮图由 `tools/make_gear_icon.py` 从原始 PNG 生成（白底、7 种尺寸、16×16 仍可辨认）。
> 想核对就跑 `tools\verify-shortcut-icon.ps1`：它直接按 MS-SHLLINK 结构解析 `.lnk` 字节，
> 校验上面两处，并用 `SHDefExtractIconW` 把索引 0 和索引 -101 各导出成 PNG 供肉眼比对。

双击它**只做两步**，与 macOS 菜单栏图标（`Squirrel --sync` → `--reload`）同一策略：

```
RIME /sync  →  /deploy
```

文件的拉取与上传由后台服务完成，文本配置的合并写回仍由 GUI 完整同步负责。快捷方式只补齐服务在
session 0 里做不到的那两步（读用户配置、让输入法重载），所以双击**不等网络**。

容错：`/sync` 失败直接报错（消息框 + 日志）；`/deploy` 失败同样报错 ——
它本来就是"让词库生效"的最后一步，静默失败等于什么都没做。

> **已知取舍**：`Sync/Others/` 里的自定义短语、词典不再由快捷方式写回 `RIME/`。
> 另一台机器改了 `*.custom.yaml` 这类"其他同步文件"时，本机要靠 GUI 的
> 「同步词库」按钮（走完整的 `--sync-now`）才会落到 `RIME/` 目录。
> 词库本身不受影响：`*.userdb.txt` 的合并发生在 RIME 的 `/sync` 内部。

`--notify` 只在结束时弹一个结果消息框（双击启动没有控制台，`println!` 会石沉大海）；
从命令行调用可以省略。

### 命令行（无界面）

带参数启动时不会打开窗口，结果写入配置目录下的 `RimeSync.log`：

```
--sync-now             执行一次完整「同步词库」：拉取 → 并集 → 写回 → 上传
                       → RIME /sync → /deploy（GUI 按钮走的就是这条）
--rime-sync            只做 RIME 自带的用户资料同步 + 重新部署
                       （桌面快捷方式用的就是它；文件搬运由后台服务负责）
--round                只执行一轮后台搬运后退出（不合并、不部署）
--scheduled-round      供 macOS 后台代理调用：后台搬运关闭时跳过，否则跑一轮
--install-service      安装并启动 Windows 后台服务（需要管理员权限）
--uninstall-service    停止并卸载 Windows 后台服务（需要管理员权限）
--install-agent        安装 macOS 后台代理（launchd 定时搬运，不需要程序常开）
--uninstall-agent      卸载 macOS 后台代理
--install-shortcut     在程序所在目录创建「同步词库」快捷方式
                       （Windows 为 .lnk，macOS 为 AppleScript .app）
--uninstall-shortcut   删除该快捷方式
--run-service          由服务控制管理器调用，不应手工执行
--service-status       打印后台服务状态
--notify               结束时弹消息框（快捷方式用；命令行下不需要）
--app-dir <目录>       指定配置目录（默认是程序所在目录）
--help                 显示帮助
```

退出码：`0` 成功，`1` 执行失败，`2` 参数错误。

> 更新已安装的服务时，程序会**自动停止并重新启动它**。这是必须的：
> `change_config` 只改 SCM 里的登记，不会影响已经在跑的进程 ——
> 不重启的话会出现"`sc qc` 看着是对的，服务日志里却指向旧配置目录"。

## 合并与冲突

并集是幂等的，所以双端几乎同时点「同步词库」时**丢不掉**：被覆盖的那台本地源文件里
仍有自己的词，下次再点同步就会并回来，只是延迟一轮。

newest 型文件则会真的丢内容，因此必须配三向判断。程序本地维护
`.rime-sync/sync-index.json`，记录每个文件**上次同步后的内容 hash**：

| 本地 vs 基线 | Others vs 基线 | 判定 | 动作 |
| --- | --- | --- | --- |
| 相同 | 不同 | 他机改了 | Others → 覆盖本地源 |
| 不同 | 相同 | 本机改了 | 本地源 → 覆盖 Others |
| 都不同 | 都不同 | **冲突** | 保留本地，远端副本另存为 `<name>.conflict-<时间戳>.<扩展名>` 并提示 |
| 都相同 | — | 无变化 | 跳过 |

同一份远端内容只会提示一次，不会每轮都生成新副本。

## 上传前校验

`Others/` 是共享文件，两台设备若几乎同时点「同步词库」，后传的会覆盖先传的。
程序按服务端能力选档：

| 档 | 服务端能力 | 做法 |
| --- | --- | --- |
| A | ETag + `If-Match` | PUT 带 `If-Match`；返回 412 → 本轮不传并提示，留到下一轮 |
| B | 只有 ETag | 上传前重取一次 ETag，与本轮开始时不同 → 本轮不传 |
| C | 都没有 | 用 `getlastmodified`+`getcontentlength` 粗筛；仍不可信则下载全文比 hash |

C 档是兜底：**即使服务端一点元数据都不给，方案仍然可用**，只是多一次全量下载。
有 `MOVE` 时用 `.tmp` → `MOVE` 原子发布；没有则在 PUT 后校验 hash。

## 删除与同步列表

**A 移除一个同步文件，只影响 A，B 完全不受影响。**

- 移除 = 写进 A 本地的排除集，A 不再合并、不再搬运该文件；
- **A 不删除 `Others/` 里的该文件** —— 那是公共结果，可能正是 B 在用的；
- 想让某文件在所有设备消失，就各设备各自移除一次。

已知限制：词条级删除传播不了。从 `my_words.dict.yaml` 删掉一行，只要文件还在同步列表，
并集就会从 `Others/` 把它拉回来。这与 userdb 的行为不一致 —— userdb 的删除**能**跨设备
传播（librime 负 commits + `abs(commits)` 规则）。

## 从 v0.2.5 升级

首次启动会自动迁移，无需手动操作：

- 本地 `Sync/WebDAV/` 的内容 → `Sync/Others/`，然后删除该目录
  （否则 librime 会把它当成一台设备参与合并）；
- 远端 `webdav-sync.zip` 与更早的散文件 → `Sync/Others/`，并删除远端旧文件；
- INI 里的 `[sync_files]` 列表 → `.rime-sync/sync-list.json`，迁移后自动清理 INI 段落。

迁移完成后还需要**做一次「设备设置」**：新版每个设备都要有 `session_token` 才能同步。
旧 ID 可以继续用（在远端还没有 `<ID>/` 目录时会被直接抢占注册），
如果该 ID 已被别的设备注册过，就需要密码验证接管或换一个 ID。

## 日志

日志同时显示在主窗口，并追加写入程序目录下的 `RimeSync.log`
（macOS 为当前配置目录）。每条记录带 `YYYYMMDDHHmmss` 前缀。日志不会记录任何密码。

**GUI 与后台服务写的是同一个文件，窗口也是从该文件增量读取的**，
所以服务在后台搬运时，日志窗口里同样能看到它写的行（谁先写都不影响）。
窗口只保留末尾约 512 KB。

服务每轮都会留下明确的起止记录，便于确认它是否真的在工作：

```text
20261003220612 收到唤醒请求，开始一轮后台搬运（间隔 10 分钟）。
20261003220612 后台搬运完成：下载 2 项，上传 1 项。
```

## 常见问题

### 提示"尚未完成设备设置"

说明 `.rime-sync/device.json` 还没有 `session_token`。请到「设备设置」注册 ID 与密码，
或用「导入配置」恢复之前的设备身份。

### 提示"此 ID 已被其他设备接管"

`owner.json` 里的 `session_token` 与本机不一致：该 ID 已在别处被接管。
请换一个 ID，或导入最新的设备配置。

### 提示"另一个同步实例正在运行"

同一时刻只能有一个同步在跑（后台轮次与手动同步共用锁）。稍后重试；
若确认没有其它实例，可能是上次异常退出后锁未释放，重启程序即可。

### 远端已有同名 ID，怎么办

程序**不会**自动占用。填好密码后重试「注册 / 验证并接管」；
忘记密码就用「强制接管」并重设新密码；不想复用就换一个 ID。

### 找不到 `WeaselDeployer.exe`

Windows 会先查找自身所在目录，再查找小狼毫的注册表安装位置。仍找不到时可在 INI 中指定：

```ini
[rime]
deployer_path=C:\Program Files\Rime\weasel-x.x.x\WeaselDeployer.exe
```

### WebDAV 测试失败

检查地址是否指向可读写文件夹、用户名密码是否正确、账号是否有创建/上传/下载/删除权限、
服务器是否支持所用方法、防火墙或代理是否阻止连接。只读共享会在 `MKCOL`/`PUT` 阶段失败。

### 同步被中断后目录里仍有文件

这是正常的安全保护：只有在远端完整写入成功后才更新本地基线，不会因为中断而丢数据。
解决连接问题后重新点「同步词库」即可。

## 从源代码构建

安装当前稳定版 Rust 工具链（需要 1.85 以上，edition 2024）后运行：

```bash
cargo test
cargo build --release
```

Windows 输出 `target/release/RimeUserDictSync.exe`；
macOS 可运行 `./build-macos.sh` 一次生成 ARM64、x86_64 与 Universal 2 三个 `.app` 压缩包。

源码中的 `Program.cs`、`MainForm.cs`、`UserDictSync.csproj` 与 `build.cmd`
是旧版 C# 实现，仅作参考保留；构建入口是 `Cargo.toml`。

## 自动构建和发布

`.github/workflows/release.yml` 在推送 `v*` 标签时，用 GitHub 托管的 Windows 与 macOS
构建机完成测试、编译和打包，两个平台全部成功后上传到同一个 GitHub Release。

发布新版本时，先把 `Cargo.toml` 的版本号改好并提交，再推送匹配的标签：

```bash
git tag v0.3.0
git push origin main
git push origin v0.3.0
```

标签必须是 `v` 加 `Cargo.toml` 中的版本号，否则构建会主动失败。

## 数据安全建议

- 首次使用前备份 RIME 用户词库与远端数据。
- 为程序使用专用的 WebDAV 文件夹。
- 不要同时运行多个本程序实例。
- 不要在同步过程中关闭计算机或手动移动 `Sync` 目录。
- 不要公开包含 WebDAV 凭据的 `RimeUserDictSync.ini`。
- 定期检查 `RimeSync.log`。

## 已知限制

1. **userdb 删除是条件性的**：对方 `commits` 绝对值更大时，你的删除压不过去，
   且会被反向复活（librime 刻意设计）。
2. **生效有延迟**：后台只搬运，要等下一次手动「同步词库」才生效。
3. **做不到秒级**：RIME 没有热更新 userdb 的接口，生效必须重新加载。
4. **词条级删除不传播**（文本词典）；userdb 的删除可以传播。
5. **服务端可见明文**：介意的话需要换支持端到端加密的服务端。

## 开源许可与致谢

本项目采用 [GNU General Public License v3.0](LICENSE)。

程序调用 [小狼毫（rime/weasel）](https://github.com/rime/weasel) 的官方部署程序，
并复用其 `weasel.ico` 图标资源。感谢 Rime 与相关项目的开发者和贡献者。

## 捐赠支持

如果这个工具对你有帮助，可以通过下面的二维码支持项目维护：

![捐赠二维码](assets/donation.png)
