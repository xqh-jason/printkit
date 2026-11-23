# printkit

在用户本机把声明式 JSON 转成 PDF 或直接出纸。文档就是一段 JSON；`printkit`
用系统 WebView 渲染，然后写出 PDF 或交给打印机。不需要浏览器运行时，不经过后端，
也不连云端。

[English](README.md) | 简体中文

[![CI](https://github.com/xqh-jason/printkit/actions/workflows/ci.yml/badge.svg)](https://github.com/xqh-jason/printkit/actions/workflows/ci.yml)
[![Release](https://github.com/xqh-jason/printkit/actions/workflows/release.yml/badge.svg)](https://github.com/xqh-jason/printkit/actions/workflows/release.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE-MIT)
[![Platform: macOS arm64 | Windows preview](https://img.shields.io/badge/platform-macOS%20arm64%20%7C%20Windows%20preview-lightgrey.svg)](#平台支持)

## 为什么用 printkit

- **天生只在本地跑。** 服务只监听 `127.0.0.1`。网页在谁的电脑上打开，就在谁的
  打印机上出纸；没有服务端中转，也没有需要配置的局域网或云端监听。
- **文档就是 JSON。** 页面、纸张、块、份数都写在同一个文档里，渲染前先按
  `schema/document-v1.json` 校验，坏文档拿到错误码，而不是半张打印纸。
- **渲染交给操作系统。** macOS 用 WKWebView，Windows 用 WebView2；不打包
  Chromium，也没有需要自己维护的 headless 浏览器。
- **桌面应用是可选的。** 命令行加本地服务就是完整产品；桌面应用只是在此之上
  增加一个管理窗口（900×633，macOS overlay 标题栏，50px header；默认尺寸由侧栏承载 toggle/logo，header 只显示面包屑），并启动同一个服务。侧栏有原创 fox mascot（0.7s、支持 reduce-motion）；端口只在侧栏 footer 显示实时 `127.0.0.1:{port}`，语言选择在 Settings，config JSON/doctor/日志仍保留原始端口诊断。
- **Overview 就是服务看板。** 四个 KPI（今日成功打印，一次成功打印请求计 1，附今日 Done/Failed/Cancelled；7 天打印任务；打印失败率（失败 /（完成 + 失败），不含取消/待处理）；系统打印机数）、7 天打印页数柱图（全部任务）、状态分布、常用打印机与最近打印任务（全部状态）；summary 的 `pending` 是查询窗口内的待处理总数（v1 必填；旧服务缺失时按 0 读取），不是今日计数，也没有今日同名计数；完成 = 总数 - 失败 - 取消 - 待处理；
  records 插件关闭或服务不可用时不会发起 records 调用，并明确显示
  off/down/loading/empty/error，而不是用 0 填满卡片。
- **能力做成插件，且默认关闭。** 新安装使用 `core` preset：打印、预览、出 PDF
  与管理面可用；记录、日志是内置插件，需要时再打开。关闭插件会隐藏
  对应页面与 API 路由，但保留数据。

## 工作方式

```mermaid
flowchart LR
  Client[浏览器页面或桌面应用] -- HTTP/WebSocket 127.0.0.1 --> Service[printkit serve]
  Service -- 操作系统打印系统 --> Printer[(打印机)]
```

## 安装

macOS 11 及以上、Apple silicon，不需要管理员权限。发布产物分两类：

**分发包**（无需安装步骤）：

- `printkit-macos-arm64.tar.gz` —— 命令行二进制。与
  `printkit-schema-macos-arm64.tar.gz` 解压到同一目录，二进制从解压出的
  `schema/` 读取契约。
- 本版本不提供 Windows 命令行压缩包。

**安装包**（仅桌面应用）：

- `printkit-desktop-macos-arm64.dmg` —— 拖入「应用程序」后启动，它自己拉起
  本地服务。ad-hoc 签名、未公证，首次启动可能需要右键 → 打开。
- Windows：由 release workflow 构建
  `printkit-desktop-windows-x64-setup.exe` (NSIS) 与
  `printkit-desktop-windows-x64.msi`。两者都未签名，也从未在真实 Windows
  主机上安装过。

每个 release 还附带 `SHA256SUMS`、`sbom.cdx.json`、`THIRD-PARTY-NOTICES.md`、
`LICENSE-MIT`、`LICENSE-APACHE` 与 `OFL.txt`；JSON Schema 契约与随包字体许可
放在 schema 压缩包内。

## 快速开始

桌面：打开 dmg，把应用拖进「应用程序」并启动。它会在 `127.0.0.1:19080` 起服务，
用 `0600` 的 `admin-token` 自动登录；全部管理界面都在这个应用里。

```sh
tar -xzf printkit-macos-arm64.tar.gz
tar -xzf printkit-schema-macos-arm64.tar.gz
./printkit pdf -i document.json -o out.pdf     # 只出 PDF，不需要打印机
./printkit printers
./printkit print -i document.json --printer "My Printer"
```

在浏览器页面里调用（需先把页面 origin 加入白名单，见
[接入指南](docs/web-integration.zh-CN.md)）：

```ts
const engine = await createEngine({ origin: window.location.origin });
await engine.print(document); // → { jobId }
```

去重是尽力而为，不是投递保证：`requestId` / `Idempotency-Key` 在单个服务进程内最多记住
600 秒、1024 条、约 1 MiB，服务重启即清空。不能重复纸张的调用方需自备业务 id。

除 `serve` 与 `schema` 外，命令都支持 `--json`；具体选项看
`printkit <command> --help`。完整参考见 [docs/cli.zh-CN.md](docs/cli.zh-CN.md) · [English](docs/cli.md)。

## 平台支持

| 能力 | macOS arm64 | Windows x64 |
|---|---|---|
| PDF 输出 (`pdf` / `doc.exportPdf`) | ✅ | ⚠️ |
| 图形打印 | ✅ | ⚠️ |
| 原始设备字节 (ESC/POS, TSPL, ZPL, CPCL) | ✅ | ⚠️ |
| 预览窗口 | ✅ | ⚠️¹ |
| 批量 / `doc.printBatch` | ✅ | ⚠️ |
| `doc.printPdf` 直通 | ✅ | ⚠️ |
| 双面 | — ² | — ² |
| 分发包 | tar.gz | 无 |
| 安装包 | dmg | NSIS/MSI ³ |

✅ 本版本已实现。⚠️ 已实现，但未在真实 Windows 硬件上验证。— 不提供。

1. Windows 预览窗口有原生 Print 按钮与 Ctrl+P；页码与缩放控件尚未实现。
2. 两个平台都在真机双面设备验证前用 `E_UNSUPPORTED` 拒绝双面请求。这是有意
   为之：双面任务不会静默变成单面。
3. 由 release workflow 构建，未签名，未在真实 Windows 主机安装。缺少 WebView2
   Runtime 时由 NSIS 安装包下载 bootstrapper。

Windows 渲染池是动态的：逻辑核数的一半，至少 1 个、最多 4 个 WebView2 worker
（`render.workers` 只能下调），需要 WebView2 Runtime 1.0.1518.46 或更高版本；
混合纸张文档在那里逐页渲染后再合并。这些路径已实现，但未在 Windows 实机验证。

## 插件

二进制自带两个内置能力插件。CLI 或 `serve` 的数据目录保持 `preset = core`，
两个插件默认关闭；桌面应用首次自建启动时启用 `observability` 与 `records`，
且从不覆盖已存在的 `plugins.toml`。用 `printkit plugin enable <id>` 或桌面
Plugins 页开关：

| 插件 | 增加 | 拥有 |
|---|---|---|
| `records` | Records 页、`records.list/stats/summary/export`、`/records/*`、`printkit stats` | `print_record` 表 |
| `observability` | Logs/Jobs 页、`trace.subscribe/tail`、`logs.export`、CLI `printkit logs` | JSONL 跟踪 sink 与 `logs` CLI 门控 |

关闭 `observability` 会停止写跟踪事件，并且不改动已有 JSONL 文件：gzip 压缩与保留期清理
都不再运行；重新开启后，下一次写入恢复轮转。

“插件管理”是插件组首项，所有插件关闭时也常驻。每个插件行显示页面 chips：
已启用页面可跳转，关闭的页面不可点，外部插件或无页面显示“无页面”。内置插件
显示桥权限不适用；外部页面按自身配置/打印机/记录摘要显示允许/禁止/未声明，
缺失声明不推断为允许或禁止。

Jobs 页原位轮询：卡片按首次观察顺序稳定排列，不重播入场/呼吸动画，选中详情不重挂载，
只有首屏显示 loading 占位；全局页面淡入仍只播放一次。

Records 的 from/to、状态、打印机、标记筛选同时作用于 summary 和 list，
KPI/成功率/今日成功打印数与表格一致；load-more 丢弃过期页和重复点击，loading 或服务不可用时导出禁用。

Settings 有数据保留卡：`config.toml` 的 `[retention].days` 默认 90（1–3650），共同控制
JSONL 日志与 `print_record`；没有新键时保留 legacy `[log]`/`[records]` 显式值。
保存原子替换并保留其它配置，卡片显示 source 与是否需要重启。

core 打印不依赖任何插件：两个全关时 `pdf`、`print`、`printBatch`、`printPdf`、
`preview`、`raw` 与 `exportPdf` 仍可用。旧 `plugins.toml` 会一次性迁移为
`preset = core` 并留下 `plugins.toml.bak-core`；`plugins.toml` 损坏时保持原样，加载器回退到
`plugins.toml.bak` 做诊断，服务以 core-only 运行直到文件修复。升级跨过模板删除时会先备份到
`printkit.db.bak-before-v8` 再 drop 旧表；若升级时校准表仍有数据，会先写
`printkit.db.bak-before-v9`。

## 文档

| 文档 | 内容 |
|---|---|
| [docs/cli.zh-CN.md](docs/cli.zh-CN.md) · [English](docs/cli.md) | 命令、选项、数据目录、`serve` 端点与鉴权 |
| [docs/web-integration.zh-CN.md](docs/web-integration.zh-CN.md) | 浏览器页面接入：协议、SDK、能力位 |
| [CONTRIBUTING.md](CONTRIBUTING.md) | 构建、门禁、CI、打包 |
| [SECURITY.md](SECURITY.md) · [CHANGELOG.md](CHANGELOG.md) · [docs/release-checklist.md](docs/release-checklist.md) | 漏洞报告、变更日志、发布清单 |
| [apps/desktop/README.md](apps/desktop/README.md) | 桌面应用与服务生命周期 |
| [packages/sdk-ts/README.md](packages/sdk-ts/README.md) | TypeScript SDK |
| [SUPPORT.md](SUPPORT.md) | 社区与商业支持 |

## 安全模型

- `serve` 只监听 `127.0.0.1`，没有局域网、公网或云端模式。
- `<data_dir>/admin-token` 在首次启动时创建，仅所有者可读（Unix 下 `0600`，
  Windows 下用户专属 DACL），用于 `/admin/*` 鉴权。
- 桌面应用从不直接打开数据库：它自己读 token，通过回环 HTTP 与服务通信，token
  不进入 UI、IPC、日志或 Tauri 命令返回值。
- 浏览器调用方受精确 `Origin` 白名单约束：白名单内的 origin 无需 token 即可获得
  print 权限；`raw` 与 `admin` 需要经 `/admin/tokens` 签发的 token。
- `/admin/*` 是纯 JSON 控制面，没有 HTML 页面。`doc.exportPdf` 返回签名
  `url` 与 `expiresAt`（10 分钟 capability），对应
  `GET /exports/<path>?exp=<unix>&sig=<hmac>`。该路由只读、只接受回环
  `Host`，且限定在 `<data_dir>/exports/`；未签名、已过期或与文件不匹配的链接
  返回 `403 E_AUTH`。不要构造或分享裸 `/exports/<path>` URL。

## 开发

`./scripts/dev.sh setup` 检查工具链，`./scripts/dev.sh desktop` 用临时数据目录
同时跑服务和桌面应用。需要 Rust 1.89+、Node.js 20.19+/22.12+ 与 pnpm 11；
`bash scripts/gates.sh` 跑本地门禁。详见 [CONTRIBUTING.md](CONTRIBUTING.md)。

构建不是逐字节可复现的。`BUILD-INFO` 记录源码 commit、`rustc`、
`SOURCE_DATE_EPOCH` 与 `reproducible=not-byte-reproducible`；详见
[docs/release-checklist.md](docs/release-checklist.md#reproducibility)。

## 版本策略

1.0 之前：次版本可能改变 CLI 或服务行为，补丁版本只修 bug。
`schema/document-v1.json` 与 `protocol-v1.json` 独立版本化；首次公开发布后，
各自的 version 1 在同一大版本内保持兼容。MSRV 为 Rust 1.89，只在次版本提升。
弃用会在 [CHANGELOG.md](CHANGELOG.md) 中提前至少一个次版本说明。`0.2.4` 仍是
1.0 之前的版本，因此首次公开发布前删除的方法（如 `calib.set`/`calib.list`）
属于预发布 breaking change，不提供兼容层。每个 release tag 必须有对应的
changelog 条目；GitHub release notes 由该 tag 生成。

## 许可证

以下两种许可证任选其一：[Apache License, Version 2.0](LICENSE-APACHE) 或
[MIT License](LICENSE-MIT)。第三方组件与随包字体列在
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md)，该文件由 `sbom.cdx.json` 生成。

SPDX-License-Identifier：`MIT OR Apache-2.0`。仓库根 `LICENSE` 带同一份摘要。
GitHub 的许可证识别器可能只读取一个文件而报成 Apache-2.0；本项目实际是双许可。
