# 青鸟 · 飞书消息助手

> 蓬山此去无多路，青鸟殷勤为探看。—— 李商隐《无题》

**青鸟**，西王母的信使神鸟，中国古代神话中专司传递消息的灵禽。以此为名，愿它帮你把每一条消息稳妥送达飞书群。

一个跨平台桌面小工具：向**飞书自定义机器人（Webhook）**发送各种格式消息。支持 macOS 与 Windows 10/11。

## 功能

| 消息类型 | 说明 |
|---|---|
| 文本 text | 纯文本，支持插入 `@所有人` / `@指定成员` |
| 富文本 post | 多段落，支持超链接、图片（image_key）混排 |
| 图片 image | 通过 image_key 发送图片 |
| 消息卡片 interactive | 内置常用模板，JSON 可视化编辑 |

其他特性：

- **多机器人管理**：保存多个 Webhook（地址 + 签名密钥），下拉一键切换
- **自动签名**：配置密钥后自动计算 `timestamp + sign`（HMAC-SHA256），无需手工处理
- **发送历史**：自动记录最近 50 条，点击一键回填重发
- **简洁界面**：单一窗口，类型 Tab 切换，快捷键 `⌘/Ctrl + Enter` 发送
- **本地配置**：机器人与历史保存在本地配置文件，不经过任何第三方服务

## 技术栈

- [Tauri 2](https://tauri.app)（Rust + WebView）—— 安装包仅数 MB，内存占用低
- 原生 HTML / CSS / JavaScript，无前端框架

## 本地开发

```bash
npm install
npm run tauri dev     # 开发模式
npm run tauri build   # 构建安装包
```

## 构建 Windows 版本

> 本项目已配置 `offlineInstaller` 模式：构建时将 WebView2 完整离线安装包（约 127MB）嵌入安装程序，**目标机器无需联网**即可安装运行（适合内网环境）。

两种方式任选：

1. **在联网的 Windows 机器上构建**：安装 [Rust](https://rustup.rs) 和 [Node.js](https://nodejs.org)（≥ 18），执行 `npm install` 后 `npm run tauri build`，产出 `.msi` / `.exe` 安装包（约 130MB），拷贝到内网机器安装即可。构建过程需要联网一次（下载 WebView2 离线包与依赖）。
2. **GitHub Actions 自动构建**：推送 `v*` 标签到 GitHub 仓库，`.github/workflows/build.yml` 会自动构建 macOS 与 Windows 安装包并发布到 Release（打勾 `releaseDraft` 后可在 Release 页面下载）。

> 提示：安装时如遇权限提示，请使用管理员账号执行一次（安装 WebView2 运行时需要系统级安装，仅首次安装时需要）。

## 使用说明

1. 在飞书群 → 设置 → 群机器人 → 添加**自定义机器人**，复制 Webhook 地址与签名密钥。
2. 打开青鸟，点右上角「管理」添加机器人。
3. 选择消息类型，编辑内容，点「发送」或按 `⌘/Ctrl + Enter`。

### 富文本行内语法

| 写法 | 效果 |
|---|---|
| `[文字](https://… )` | 超链接 |
| `![说明](image_key)` | 内嵌图片 |

### 关于图片 / 文件

- 飞书 Webhook 自定义机器人**不支持**直接上传文件，也不支持 `file` 消息类型。
- 发图片需先通过飞书自建应用「上传图片」接口（`POST /open-apis/im/v1/images`）拿到 `image_key`，再填入青鸟发送。
- 发文件可将文件上传到云空间后，把分享链接放进文本 / 富文本消息。

## 配置存储位置

- macOS：`~/Library/Application Support/com.qingniao.app/qingniao.json`
- Windows：`%APPDATA%\com.qingniao.app\qingniao.json`
