# 青鸟 · 飞书消息助手

> 蓬山此去无多路，青鸟殷勤为探看。—— 李商隐《无题》

**青鸟**，西王母的信使神鸟，中国古代神话中专司传递消息的灵禽。以此为名，愿它帮你把每一条消息稳妥送达飞书群。

一个跨平台桌面小工具：向**飞书自定义机器人（Webhook）**发送各种格式消息，并在两台机器之间**端到端加密**地互传文件。支持 macOS 与 Windows 10/11。

<img width="3800" height="1884" alt="image" src="https://github.com/user-attachments/assets/b8cb0133-9806-4222-b337-4d552297f603" />

## 功能

### 消息

| 消息类型 | 说明 |
|---|---|
| 文本 text | 纯文本，支持插入 `@所有人` / `@指定成员` |
| 富文本 post | 多段落，支持超链接、图片（image_key）混排 |
| 图片 image | 通过 image_key 发送图片 |
| 消息卡片 interactive | 用 <code>&#96;&#96;&#96;card</code> 代码块写卡片 JSON；也可简单写「标题 + 正文」自动成卡 |

- **多机器人管理**：保存多个 Webhook（地址 + 签名密钥），下拉一键切换
- **自动签名**：配置密钥后自动计算 `timestamp + sign`（HMAC-SHA256），无需手工处理
- **智能识别**：输入或粘贴内容时自动判断文本 / 富文本 / 图片 / 卡片，可在「偏好」里调整识别优先级
- **发送历史**：自动记录，点击一键回填重发
- **消息记录**：发出的消息、图片、文件与取回的文件按时间汇在一条流里
- **快捷键**：`⌘/Ctrl + Enter` 发送（可改为 `Enter`），放大编辑器支持标题与 Markdown 工具条

### 文件传输

在内网机与外网机之间互传文件（两端仅能访问飞书域名，无直连、无中转服务器）：

- **端到端加密**：文件在发送方处理，只有配过对的接收方机器能还原，飞书与群里的其他人拿到的都是密文
- **群里只出现一条取回链接**：拖入文件或从输入框选择文件，群里收到一张取回卡片，链接 **10 分钟内有效**
- **点击即取回**：接收端打开链接，由本机常驻服务逐片取回，直存下载目录（可配置，默认系统下载目录）
- **应用层分片**：16 MB/片上传飞书云空间，单文件程序上限 **100 MB**
- **配对密钥**：首次使用生成 256 位主密钥存入系统凭据库；第二台机器通过「导出配对 / 导入」完成配对；支持轮换（旧密钥保留 30 天）
- **下载即删**：取回校验通过后删除云端分片，释放免费租户空间
- **云端清理**：发送端在约 40 分钟宽限期后自动清理未取回的分片；另有每周一次的孤儿扫描，清理云端超过 48 小时的残留
- **仅限电脑端取回**：取回依赖本机常驻服务，手机端无法完成

### 其他

- **常驻后台**：关闭窗口后本地服务继续监听（托盘 / 菜单栏常驻），群里的链接才点得开；服务端口可在配置页调整（缺省 9876）
- **本地存储**：机器人与历史保存在本地；传输主密钥存在系统凭据库，不经过任何第三方服务

## 技术栈

- [Tauri 2](https://tauri.app)（Rust + WebView）—— 体积小、内存占用低
- 原生 HTML / CSS / JavaScript，无前端框架
- Cargo workspace 三 crate：
  - `src-tauri`：Tauri APP（二进制 `qingniao`，托盘、常驻服务、传输引擎适配）
  - `crates/qingniao-core`：共享核心（消息组装、签名、发送、配置持久层、文件传输协议），零 Tauri 依赖
  - `crates/qingniao-cli`：命令行（二进制 `qingniao-cli`，安装后名为 `qingniao`），与 APP 共用核心

## 本地开发

```bash
npm install
npm run tauri dev     # 开发模式
npm run tauri build   # 构建安装包
```

测试：

```bash
cargo test            # 全 workspace（core / cli / APP）
npm test              # 前端 golden 基线校验
```

## 构建与发布

### 自动化发布（推荐）

推送 `v*` 标签到 GitHub 仓库，`.github/workflows/build.yml` 会校验 tag 与四源版本一致后构建并创建 **Draft Release**，在 Release 页面下载产物。此外工作流支持在 GitHub Actions 页面手动触发（`workflow_dispatch`，指定一个既有 `v*` tag）：在默认分支最新提交上重新构建，产物覆盖到该 tag 对应的既有 Release——用于不发新版本、只重出发布包的场景。四类产物：

| 产物 | 说明 |
|---|---|
| `QingNiao_x.y.z_aarch64.dmg` | macOS（Apple Silicon）安装包，已内嵌 CLI |
| `QingNiao_x.y.z_windows-x64.zip` | Windows 便携版（APP + CLI，解压即用） |
| `qingniao-cli-x.y.z-macos-arm64.zip` | 独立 CLI（macOS arm64） |
| `qingniao-cli-x.y.z-windows-x64.zip` | 独立 CLI（Windows x64） |

### 本地构建 Windows 安装程序

CI 默认只出 Windows 便携 zip。如需生成 `.msi` / `.exe` 安装程序（已配置 `offlineInstaller`：嵌入 WebView2 完整离线安装包，目标机器无需联网，适合内网）：

1. 在 Windows 机器上安装 [Rust](https://rustup.rs) 和 [Node.js](https://nodejs.org)（≥ 18）；
2. 执行 `npm install` 后 `npm run tauri build`，产出安装包（约 130MB），拷贝到内网机器安装即可。构建过程需联网一次（下载 WebView2 离线包与依赖）。

> 提示：安装时如遇权限提示，请使用管理员账号执行一次（安装 WebView2 运行时需要系统级安装，仅首次安装时需要）。

## 使用说明

### 发消息

1. 在飞书群 → 设置 → 群机器人 → 添加**自定义机器人**，复制 Webhook 地址与签名密钥。
2. 打开青鸟，点右上角「配置 → 飞书应用」填写自建应用凭证（发图片 / 传文件需要），在顶部下拉「添加或管理机器人」添加机器人。
3. 输入内容（自动识别类型），点「发送」或按 `⌘/Ctrl + Enter`。

### 传文件

1. 首次使用，在「配置 → 文件传输 → 传输密钥」点「生成密钥」；在另一台机器上用「导出配对 / 导入」完成配对（两侧指纹一致即成功）。
2. 把文件拖入窗口，或从输入框「+ → 本地文件」选择，确认后发送；群里会出现取回卡片。
3. 在另一台已配对、且青鸟在后台常驻的机器上打开链接（或在主界面点「取回文件」粘贴链接），文件会存到下载目录。

> **严禁把配对密钥发进飞书群、云文档或任何飞书载体。** 端到端加密的前提就是飞书拿不到这把钥匙；一旦它经飞书传过一次，所有文件都等于明文。请当面输入，或走你自己信得过的渠道。

### 富文本行内语法

| 写法 | 效果 |
|---|---|
| `[文字](https://… )` | 超链接 |
| `![说明](image_key)` | 内嵌图片 |

### 消息卡片语法

在输入框写 ` ```card ` 代码块（或直接粘贴含 `config` / `header` 的卡片 JSON），青鸟会识别为交互卡片并原样发送。CLI 侧等价写法：`qingniao send --file card.json`。

### 关于图片 / 文件

- 飞书 Webhook 自定义机器人**不支持**直接上传文件，也不支持 `file` 消息类型。
- 发图片 = 用自建应用调用「上传图片」接口（`POST /open-apis/im/v1/images`）换 `image_key` 再发送；APP 内直接选图即可，CLI 用 `upload-image` / `--image`。
- 发文件走「文件传输」通道，群消息里只承载取回链接，不占用 Webhook 请求体。

## Agent CLI（qingniao 命令行）

让 AI Agent 通过命令行发送消息、传文件，与 APP 共用同一套核心（行为逐字一致）：

- **安装**：APP「配置 → Agent」一键安装（macOS 符号链接到 `~/.local/bin/qingniao`，Windows 复制到
  `%LOCALAPPDATA%\qingniao\bin\`）；或在 Release 下载 `qingniao-cli-<版本>-<平台>.zip`，
  解压后把 `qingniao` 放进 PATH
- **技能**：APP「配置 → Agent」一键托管同步到 `~/.agents/skills`、`~/.claude/skills`、`~/.codex/skills`；
  或由 Agent 自行执行 `npx skills add fxfboy/qingniao`
- **文档**：`skills/qingniao/SKILL.md`（命令、`--json` 契约、退出码、URL 安全策略）

主要命令：

```bash
qingniao doctor                      # 环境自检（无副作用）
qingniao send "部署完成"              # 发消息（自动识别类型）
qingniao upload-image --file a.png   # 上传图片换 image_key
qingniao bot ls / add / use / rm     # 机器人管理
qingniao config show / path          # 配置概览
qingniao history                     # 发送记录（与 APP 互通）
qingniao transfer send ./pkg.tar.gz  # 发文件（生成取回链接卡片）
qingniao transfer recv "<取回链接>"   # 取回文件
```

所有命令支持 `--json` 与统一退出码（0 成功 / 1 用法或配置错误 / 2 失败）：成功路径在 stdout 输出单个机器可读 JSON，诊断日志与失败信息走 stderr。

> macOS 单独下载的 CLI 未做签名（unsigned），首次运行如被 Gatekeeper 拦截：
> `xattr -d com.apple.quarantine $(which qingniao)`

## 配置与存储位置

- macOS：`~/Library/Application Support/qingniao/`
- Windows：`%APPDATA%\qingniao\`

其中：

- `qingniao.json`：机器人、飞书应用凭证、发送历史与传输偏好
- `qingniao.log`：运行日志（与配置文件同目录），记录配置读写、消息发送、图片上传、文件传输、连接测试等关键操作与错误；前端未捕获的异常也会写入。日志中 app_id 与 webhook 密钥均做了脱敏，不会记录明文密钥
- `transfer/`：文件传输的配额、已消费记录等状态文件
- 传输主密钥 K：存放于 **OS 凭据库**（macOS Keychain / Windows Credential Manager），不写入配置文件
