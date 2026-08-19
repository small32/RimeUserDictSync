# RIME 用户词库同步

本项目已使用 Rust 重构为跨平台 RIME 用户词库同步工具。在 Windows 上配合小狼毫
（Weasel）使用，在 macOS 上配合鼠须管（Squirrel）使用。程序调用对应平台 RIME
前端自带的用户资料同步和重新部署功能，并通过 WebDAV 在不同设备之间同步用户词库。

## 主要功能

- Windows 调用 `WeaselDeployer.exe /sync`，macOS 调用 `Squirrel --sync`。
- 通过 WebDAV 下载和覆盖上传同步资料。
- 合并 `cn_dicts`、`en_dicts` 中的词库文件。
- 同一词条存在不同权重时，保留权重较大的记录。
- 按修改日期同步 `wanxiang-lts-zh-hans.gram`（如果有这个文件的话 ##这个文件是万象拼音的AI智能长句输入的模型文件）。
- 合并后在 Windows 调用 `/deploy`，在 macOS 调用 `Squirrel --reload`。
- 提供同步进度、实时日志、停止同步和 WebDAV 读写测试。
- 同一套 Rust 源码可在 Windows、macOS 上原生编译；Windows 发布为单文件 EXE。

## 运行环境

- Windows 10/11 并安装小狼毫；或 macOS 并安装鼠须管。
- WebDAV 服务需要支持 `PROPFIND`、`MKCOL`、`GET`、`PUT` 和 `DELETE`。
- Windows 版的程序所在目录必须具有读写权限。

建议不要将程序放在 `Program Files` 等需要管理员权限才能写入的目录中。

macOS 版的配置、日志和同步数据保存在用户资料库，不会写入 `.app` 应用包：

```text
~/Library/Application Support/RimeUserDictSync
```

### macOS 编译与安装

安装 Rust stable 与 Xcode Command Line Tools 后，在仓库根目录执行：

```bash
./build-macos.sh
```

脚本一次生成三个压缩发布包：

- `dist/RimeUserDictSync-macOS-arm64.zip`：Apple Silicon；
- `dist/RimeUserDictSync-macOS-x86_64.zip`：Intel Mac；
- `dist/RimeUserDictSync-macOS-universal.zip`：同时支持两种架构的 Universal 2 应用。

解压所需版本，将应用拖入“应用程序”后即可启动。
未经 Apple Developer ID 签名和公证的自行编译版本首次打开时，可能需要在 Finder 中
右键应用并选择“打开”。

## 文件和目录结构

Windows 版首次运行后，会在程序自身所在目录生成以下内容：

```text
RIME用户词库同步.exe
WeaselUserDictSync.ini
RimeSync.log
Sync
├─ WebDAV
└─ <installation_id>
```

macOS 版则生成在：

```text
~/Library/Application Support/RimeUserDictSync/
├─ WeaselUserDictSync.ini
├─ RimeSync.log
└─ Sync/
   ├─ WebDAV/
   └─ <installation_id>/
```

在 Finder 中按 `Command + Shift + G`，输入
`~/Library/Application Support/RimeUserDictSync` 即可打开。Windows 与 macOS 的主要
路径差异如下：

| 内容 | Windows | macOS |
| --- | --- | --- |
| RIME 用户目录 | `%APPDATA%\Rime` | `~/Library/Rime` |
| INI 与日志 | 程序所在目录 | `~/Library/Application Support/RimeUserDictSync` |
| `Sync` 根目录 | `程序所在目录\Sync` | `~/Library/Application Support/RimeUserDictSync/Sync` |
| RIME 前端 | 小狼毫 Weasel | 鼠须管 Squirrel |
| 同步命令 | `WeaselDeployer.exe /sync` | `Squirrel --sync` |
| 部署命令 | `WeaselDeployer.exe /deploy` | `Squirrel --reload` |

各部分用途：

- `WeaselUserDictSync.ini`：保存 RIME 用户词库位置和 WebDAV 设置。
- `RimeSync.log`：保存程序工作日志。
- `Sync\WebDAV`：WebDAV 远端内容的临时本地镜像。
- `Sync\<installation_id>`：当前 RIME 安装实例的同步文件夹。

`<installation_id>` 来自所选 RIME 用户词库目录中的 `installation.yaml`，程序不会修改
该值。

## 首次配置

### 1. 指定 RIME 用户词库

点击主窗口中的“指定RIME用户词库”，选择包含 `installation.yaml` 的 RIME 用户词库
文件夹。默认用户词库位置通常是：

```text
%APPDATA%\Rime
```

macOS：

```text
~/Library/Rime
```

选择完成后，程序会：

1. 将所选文件夹地址明文保存到 `WeaselUserDictSync.ini`。
2. 读取 `installation.yaml` 中原有的 `installation_id`。
3. 保持 `installation_id` 不变。
4. 将 `installation.yaml` 的 `sync_dir` 设置为本平台的数据目录下的 `Sync`。Windows
   使用程序所在目录；macOS 使用 `~/Library/Application Support/RimeUserDictSync`：

```yaml
sync_dir: '程序所在目录\Sync'
```

macOS 示例：

```yaml
sync_dir: '/Users/你的用户名/Library/Application Support/RimeUserDictSync/Sync'
```

小狼毫实际使用的当前设备同步文件夹为：

```text
程序所在目录\Sync\<installation_id>
```

### 2. 设置 WebDAV

点击“设置WebDAV”，填写：

- WebDAV 文件夹地址；
- 用户名；
- 密码。

建议 WebDAV 地址以 `/` 结尾，并为本程序使用单独的远端文件夹，例如：

```text
https://dav.example.com/rime-sync/
```

点击“测试连接”后，程序会：

1. 在 WebDAV 中上传一个随机命名的临时文件；
2. 下载该文件；
3. 校验下载内容是否与上传内容完全一致；
4. 删除远端临时文件。

以上操作全部成功后才会显示测试通过。

WebDAV 密码以 Base64 形式保存在 INI 中。Base64 仅用于避免密码直接显示为明文，不等同
于安全加密；请妥善保护程序目录和 INI 文件。

## 使用方法

完成首次配置后，点击“开始同步”。同步期间可以通过进度条和日志区域查看当前状态。

需要中止时点击“停止同步”。程序会在当前文件或网络操作结束后停止后续步骤；如果
RIME 部署程序仍在运行，程序会终止本次启动的部署进程。

为避免词库文件在同步过程中被其他程序修改，建议同步时不要手动编辑相关文件。

## 完整同步机制

程序严格按照以下顺序执行：

### 步骤 1：第一次运行小狼毫用户资料同步

程序调用：

```text
WeaselDeployer.exe /sync
```

完成后，将 RIME 用户词库目录中的以下内容复制到
`Sync\<installation_id>`：

```text
cn_dicts\                    包含文件夹、子文件夹和所有文件
en_dicts\                    包含文件夹、子文件夹和所有文件
wanxiang-lts-zh-hans.gram
```

### 步骤 2：下载 WebDAV

程序递归读取 WebDAV 文件夹：

- WebDAV 有内容时，先清空旧的 `Sync\WebDAV` 镜像，再完整下载远端文件。
- 下载时读取 WebDAV 的 `getlastmodified`，并将其保存为本地文件修改日期。
- WebDAV 没有内容时，用当前同步资料初始化 `Sync\WebDAV` 并直接上传。

每次下载完成后，程序都会检查：

```text
Sync\WebDAV\installation.yaml
```

该文件的 `installation_id` 必须为：

```yaml
installation_id: WebDAV
```

如果字段不存在或值不是 `WebDAV`，程序会立即修正；最终会将修正后的文件覆盖上传回
WebDAV。此操作不会修改 RIME 用户词库目录中 `installation.yaml` 的
`installation_id`。

### 步骤 3：检查目录关系

程序确认本地文件夹和同步文件夹位于同一个 `Sync` 根目录：

```text
Sync\WebDAV
Sync\<installation_id>
```

目录关系不符合要求时停止同步，防止操作错误位置。

### 步骤 4：第二次运行小狼毫用户资料同步

程序再次调用：

```text
WeaselDeployer.exe /sync
```

### 步骤 5：合并词库和 gram 文件

#### `cn_dicts`、`en_dicts` 合并

程序对比 `Sync\WebDAV` 和 `Sync\<installation_id>` 下两个词库目录中的相对路径
相同文件。

词库的 YAML 文件头不参与并集。文件头是从 `---` 开始、到 `...` 结束的部分，例如：

```yaml
---
name: 8105
version: "2026-07-11"
sort: by_weight
...
```

仅对文件头之后的正文执行合并：

- 只在一侧存在的文件会复制到另一侧。
- 普通正文行采用有序并集，重复行只保留一次。
- 词条和编码相同、最后一列数字不同时，只保留数字较大的记录。
- 词条或编码不同的记录分别保留。
- 合并结果同时写入本地文件夹和同步文件夹。

例如：

```text
𤭢	cei	800
𤭢	cei	1000
```

合并后只保留：

```text
𤭢	cei	1000
```

词库文件必须是 UTF-8 文本。程序检测到非 UTF-8 或二进制文件时会停止，避免损坏数据。

#### `wanxiang-lts-zh-hans.gram` 同步

程序对比两侧同名 gram 文件的修改日期：

- WebDAV 没有该文件时，从同步文件夹复制到 `Sync\WebDAV`。
- 同步文件夹没有该文件时，从 `Sync\WebDAV` 复制过去。
- 两侧都有时，修改日期较新的文件覆盖较旧文件。
- 修改日期相同时不覆盖。

#### 重新部署

以上合并完成后，程序调用：

```text
WeaselDeployer.exe /deploy
```

重新部署成功后才继续后续步骤。

### 步骤 6：覆盖上传 WebDAV

程序将 `Sync\WebDAV` 中的所有文件递归上传到 WebDAV。同名远端文件使用 `PUT`
直接覆盖。

### 步骤 7：清理工作目录

只有步骤 6 全部上传成功后，程序才会删除以下两个目录中的所有文件和子目录：

```text
Sync\WebDAV
Sync\<installation_id>
```

两个根目录本身会保留。若上传失败或同步被停止，程序不会执行这一步，以避免本地数据
丢失。

## 日志

日志同时显示在主窗口中。Windows 保存到程序目录，macOS 保存到
`~/Library/Application Support/RimeUserDictSync`：

```text
RimeSync.log
```

每条记录前都有 `YYYYMMDDHHmmss` 格式的时间戳，例如：

```text
20260818224759 步骤 2/7：下载 WebDAV 文件: 15 个。
```

日志不会记录 WebDAV 密码。

## 常见问题

### 找不到 `installation.yaml`

请重新点击“指定RIME用户词库”，选择真正的 RIME 用户词库目录，而不是 `Sync`、
`Sync\WebDAV` 或其他备份目录。

### 找不到 `WeaselDeployer.exe`

Windows 会先查找自身所在目录，然后查找小狼毫注册表安装位置。如仍无法找到，可在
`WeaselUserDictSync.ini` 的 `[rime]` 部分手动设置完整路径：

```ini
[rime]
deployer_path=C:\Program Files\Rime\weasel-x.x.x\WeaselDeployer.exe
```

macOS 默认使用：

```text
/Library/Input Methods/Squirrel.app/Contents/MacOS/Squirrel
```

### WebDAV 测试失败

请检查：

- WebDAV 地址是否指向可读写文件夹；
- 用户名和密码是否正确；
- 账户是否有创建、上传、下载和删除文件的权限；
- 服务器是否支持本程序使用的 WebDAV 方法；
- 防火墙、代理或证书是否阻止连接。

### 同步中断后目录中仍有文件

这是正常的安全保护。程序只在 WebDAV 完整上传成功后清空工作目录。解决连接问题后
重新运行同步即可。

## 数据安全建议

- 首次使用前建议备份 RIME 用户词库和 WebDAV 数据。
- 不要同时运行多个本程序实例。
- 不要在同步过程中关闭计算机或手动移动 `Sync` 目录。
- 不要公开包含 WebDAV 凭据的 `WeaselUserDictSync.ini`。
- 定期检查 `RimeSync.log`，确认所有步骤成功完成。

## 从源代码构建

安装当前稳定版 Rust 工具链后，在目标平台运行：

```bash
cargo test
cargo build --release
```

Windows 输出：

```text
target/release/RimeUserDictSync.exe
```

macOS 可运行 `./build-macos.sh`，一次生成 ARM64、x86_64 和 Universal 2 三个 `.app`
压缩包。源码中的 `Program.cs`、`MainForm.cs` 与 `build.cmd` 暂时保留为旧版实现参考；
新的主构建入口是 `Cargo.toml`。

首次运行时程序会自动生成 INI、日志和工作目录。请勿将包含真实 WebDAV 凭据的
`WeaselUserDictSync.ini` 提交到版本库。

## 开源许可与致谢

本项目采用 [GNU General Public License v3.0](LICENSE)。

程序调用 [小狼毫输入法（rime/weasel）](https://github.com/rime/weasel) 的官方部署程序，
并复用其 `weasel.ico` 图标资源。感谢 Rime 与小狼毫项目的开发者和贡献者。

## 捐赠支持

如果这个工具对你有帮助，可以通过下面的二维码支持项目维护：

![捐赠二维码](assets/donation.png)
