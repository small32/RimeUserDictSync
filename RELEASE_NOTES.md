# v0.3.0 更新说明

按《RimeUserDictSync-同步引擎设计》重构同步引擎：**传输与生效彻底解耦**，
后台只搬运文件（零打扰），手动点「同步词库」时才合并并重新部署。

## 新增

- **快捷方式「同步词库」**：建在程序所在目录（不往桌面塞东西），双击即可执行完整的
  拉取 → 并集 → 写回 → 上传 → RIME `/sync` → `/deploy`。
  这是服务的必要补充：服务只搬运文件，`/sync` 与 `/deploy` 必须在用户登录会话里执行。
  图标是**白底黑齿轮**，以资源 ID 101 内嵌在 EXE 里；EXE 自己的图标（weasel）保持不变。
  `.lnk` 里分两处存放：`ICON_LOCATION` 只写 EXE 的**裸路径**，图标索引写在头部的
  `IconIndex` 字段（`-101`，负数表示按资源 ID 取）。
  之前几版把 `,索引` 塞进 `IconLocation` 字符串，资源管理器会把整个字符串当文件名去找，
  报「Windows 找不到文件 xxx.exe,-101」且图标为空 —— 这才是那个毛病的真正原因。
  磁盘上只有 EXE 和 `.lnk` 两个文件，不会散出额外的 `.ico`。
- **`--sync-now` 无界面完整同步**，以及 `--install-shortcut` / `--uninstall-shortcut` / `--notify`。
  双击快捷方式没有控制台，所以用消息框给出结果。
- **容错原则**：网络问题不阻止本机生效。所有 WebDAV 步骤改为尽力而为（失败只写警告），
  合并、写回、`/sync`、`/deploy` 仍然严格报错；离线时双击快捷方式也能让已拉到的内容生效。
- **Windows 后台服务**：后台搬运可以作为真正的 Windows 服务常驻运行（开机即跑、
  不必打开本程序）。以 `LocalSystem` 运行、延迟自启、失败自动重启，程序副本放在
  `%ProgramData%\RimeUserDictSync\`。
  服务**只做搬运**，不调用 `/sync`、`/deploy`，因此永远不会打断输入法。
- **无界面命令行模式**：`--round`（跑一轮后退出）、`--install-service`、
  `--uninstall-service`、`--service-status`、`--app-dir`，结果写入 `RimeSync.log`。
- **调度改为「到期判断」**：不再睡满整个间隔，而是每 30 秒醒一次判断是否到期。
  休眠唤醒后会立刻补跑，轮次耗时也不再累积成周期漂移。
- **唤醒标记**：保存 WebDAV / 同步列表设置，或点「立刻搬运一轮」，会在
  `.rime-sync/wake` 落一个标记，服务下个 tick 就立刻执行，不必等满间隔。
- **服务状态可见**：GUI 每 5 秒刷新服务状态，并显示服务当前指向的配置目录。
- **数据分层**：`Sync/<ID>/` 存放各设备 userdb 快照，`Sync/Others/` 存放文本词典的
  合并结果，两层策略不同、互不干扰。
- **后台纯搬运**：默认每 10 分钟一轮，只做「拉 Others → 拉他机快照 → 推 Others →
  推本机快照」。全程不读写 `RIME/` 根目录下的源文件，也不调用 `/sync`、`/deploy`，
  打字不受打断。
- **修复本机快照从不上传的断点**：本设备 `Sync/<ID>/*.userdb.txt` 现在会带上
  `<ID>/.index.json` 清单上传，多台设备之间才真正开始同步 userdb。
- **手动「同步词库」六步**：拉取 → 并集 → 写回 → 上传 → `/sync` → `/deploy`，
  顺序固定，不会用本地旧内容盖掉他机新内容。
- **按类型自动合并**：`*.dict.yaml` 词条级并集取大权重；`*.txt` 按首列 key 并集；
  `*.yaml`（非 dict）/`*.json`/`*.lua` 走 newest。`*.userdb.txt` 被明确禁止加入同步列表。
- **newest 三向判断**：本地维护 `.rime-sync/sync-index.json` 记录上次同步后的内容 hash，
  双端同时修改时**不静默覆盖**，远端版本另存为 `<name>.conflict-<时间戳>.<扩展名>` 并提示。
- **设备 ID / 密码体系**：`owner.json` + PBKDF2-HMAC-SHA256（60 万轮）派生串；
  首次注册做 `MKCOL` 原子抢占，远端已有同名 ID 时必须输密码验证接管，
  忘记密码可用强制接管；`session_token` 每轮校验，被接管后旧设备自动停止同步。
- **设备配置导出 / 导入**：重装前导出、装完导入即可零冲突继续用，无需接管。
- **注销设备**：删除远端 `<ID>/` 与本地 `Sync/<ID>/`，避免孤儿目录被 librime 永久合并。
- **WebDAV 能力探测与三档上传校验**：A 档 `If-Match`、B 档上传前重取 ETag、
  C 档 `size`+`mtime` 粗筛（必要时下载全文比 hash）。服务端一点元数据都不给也能用。
- **连接测试必须测写**：`MKCOL` + `PUT` + `GET` + `DELETE` 全部通过才算通过。
- **目录红线自检**：提示 `Sync/` 与 `sync` 大小写撞车、工具私有文件被误放进 `Sync/`、
  `Others/` 里混入 `*.userdb.txt`、同步文件名不是纯 ASCII 等问题。

## 修复（来自首次实机测试的日志）

- **服务拿不到配置目录，一直在 `%ProgramData%` 下空转**：原先把 `--app-dir` 写进服务的
  `lpBinaryPathName`（`sc qc` 里能看到），但 SCM 只会把 `StartService` 的参数交给
  `ServiceMain`，命令行参数根本不会传进来 —— 服务日志里能看到它只收到了服务名。
  结果服务每十分钟报一次"找不到 RIME 用户目录"，日志也写去了错误的地方。
  现在按 启动参数 → `HKLM\SOFTWARE\RimeUserDictSync\AppDir` →
  `%ProgramData%\RimeUserDictSync\app-dir.txt` → 程序目录（并告警）的顺序解析。
- **更新已安装的服务时先停再复制**：运行中的服务占着 `%ProgramData%` 里的 EXE，
  直接覆盖会报"另一个程序正在使用此文件"。
- **服务每轮写明确的起止日志**：`收到唤醒请求/定时到期，开始一轮后台搬运（间隔 N 分钟）`
  与 `后台搬运完成：下载 N 项，上传 M 项。`
- **GUI 日志窗口改为增量读取 `RimeSync.log`**：服务与 GUI 写同一个文件，
  以前窗口只显示本进程的输出，服务在后台搬运时窗口里什么都看不到。
- **快照目录里只上传 `*.userdb.txt`**：librime 的 `/sync` 会把 `user.yaml`、各方案
  `*.schema.yaml`、`*.dict.yaml` 一并复制进 `Sync/<ID>/`，旧实现把它们当快照上传，
  别的设备拉走后可能反过来覆盖对方的配置。现在只上传 userdb 快照，
  并顺手清理远端 `<ID>/` 里已经不需要的非 userdb 文件（userdb 快照一律不动，
  因为那可能是接管前那台机器的唯一一份）。
- **`Others/` 混进 `*.userdb.txt`**：旧版 `Sync/WebDAV/` 迁移时把 userdb 快照一起搬进了
  共享目录，而 librime 会把 `Others/` 当成一台"设备"参与合并。现在迁移会跳过它，
  启动时若发现残留也会挪到 `.rime-sync/quarantine/` —— 移出而不是删除。
- 服务启动时把收到的启动参数原样写进日志，`--app-dir` 缺失时给出明确警告。

## 变更

- `installation.yaml` 的 `Password` 字段保存 **KDF 派生串**而不是明文；
  `sync_dir` 指向 `<RIME 用户目录>/Sync`（不再是程序目录下的 `Sync`）。
- 工具私有状态移出同步目录：`RIME/.rime-sync/{device.json,sync-index.json,sync-list.json}`。
- 远端不再使用单个 `webdav-sync.zip`。首次运行会自动把旧压缩包与旧散文件迁移进
  `Others/`，并删除残留的 `Sync/WebDAV/`（否则 librime 会把它当成一台设备）。
- 同步文件列表改由「文件类型」决定合并方式，不再需要手工选择「按并集 / 按时间」。
- 移除「回写」开关：并集结果按设计始终写回本地源文件。
- 后台线程改为调度唤醒制；检测到 Windows 服务已在运行时自动让位，避免重复搬运。
- 版本号提升到 `0.3.0`。

## 兼容性

- 从 v0.2.5 升级：首次启动自动迁移，无需手动操作，词库数据不会丢失。
- 配置文件（INI）与同步列表自动迁移；INI 里旧的 `[sync_files]` 段迁移完成后会被清理。
