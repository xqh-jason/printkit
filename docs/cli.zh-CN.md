# printkit 命令行与本地服务

[English](cli.md) | 简体中文

一个二进制文件承担桌面应用所驱动的全部工作：把 JSON 文档渲染成 PDF、提交到打印机、发送设备原始字节、打开预览窗口，以及运行回环 HTTP/WebSocket 服务。桌面应用是可选的；下面每条命令都能脱离它运行。

## 安装布局

把两个压缩包解压到同一目录，例如 `~/printkit/`：

```text
printkit-macos-arm64.tar.gz
└── printkit                     the only executable

printkit-schema-macos-arm64.tar.gz
├── schema/
│   ├── document-v1.json         document contract
│   └── protocol-v1.json         WebSocket / HTTP contract
└── assets/
    ├── fonts/OFL.txt            the bundled font licence
    └── licenses/                third-party licence texts
```

没有安装器、没有服务注册，也不需要 `sudo`。从解压位置直接运行二进制：

```sh
./printkit version
./printkit doctor
```

发布版二进制只在**可执行文件旁边**解析 `schema/`，不会去别处找。它拒绝回退到构建它的那台机器：没有解压出来的 `schema/` 目录时，它以非零状态退出，并提示你解包压缩包或使用 `printkit schema --out DIR`。从源码构建时使用仓库检出目录。

macOS 桌面应用是单独下载的（`printkit-desktop-macos-arm64.dmg`），自带 sidecar；Windows 不发布 CLI 压缩包。

## 文档 schema

一个文档就是一个 JSON 对象，涵盖页面、纸张、块、份数与输出路由，渲染前按 `schema/document-v1.json` 校验。`pdf` 接受 `-` 作为 stdin 与 stdout，因此生成器可以管道直传：

```sh
generate-document | ./printkit pdf -i - -o - > out.pdf
```

定位或写出契约文件（`--path` 与 `--out` 互斥，且必须给出其中一个）：

```sh
./printkit schema --path        # print the directory that supplies them
./printkit schema --out DIR     # write document-v1.json and protocol-v1.json
```

## 命令

| 命令 | 用途 | 备注 |
|---|---|---|
| `pdf` | 把文档渲染为 PDF 文件 | `-i` 输入或 `-`，`-o` 输出或 `-`，`--dump-html FILE` 保留中间 HTML |
| `print` | 把文档或已有 PDF 提交到打印机 | `-p` 打印机（省略时用默认打印机），`-n` 份数，`--batch`；单个 PDF（无论是否带 `--batch`）原字节直通，只有多个输入才合并 |
| `raw` | 原样发送设备字节 | `-f` 文件或 `-`，`-p` 打印机，`--title` spooler 标题 |
| `printers` | 列出打印机 | |
| `papers` | 列出某台打印机接受的纸张尺寸 | `-p` 打印机（必填） |
| `preview` | 在原生预览窗口打开文档 | 数据目录空闲：自行启动服务；被占用：转发给它 |
| `serve` | 运行回环 HTTP/WebSocket 服务 | 无参数 |
| `doctor` | 检查渲染器、数据目录、配置与日志 | |
| `logs` | 读取本地 JSONL 跟踪事件（归 `observability` 插件） | `--trace`、`--stage`、`--field`、`--since`、`--follow` |
| `stats` | 聚合本地使用记录表（归 `records` 插件） | `--group tag\|template\|printer\|day`、`--from`、`--to`；`template` 是 `print_record.template` 的历史模板名，不是模板功能 |
| `origin` | 列出与修改允许的浏览器 Origin | `list`、`add`、`remove`；登录前校验运行中的 `serve.lock` |
| `plugin` | 列出与修改内置插件 | `list`、`enable`、`disable`、`config`、`repair`；除 `list`/`repair` 外需要运行中的服务 |
| `config` | 显示生效值以及每个值的来源 | `--path`、`--show-source`、`--validate` |
| `schema` | 写出或定位已发布的契约文件 | `--path` 或 `--out DIR` |
| `version` | 打印版本 | |

除 `serve` 与 `schema` 外，每条命令都接受 `--json`，然后向 stdout 写一个 JSON 对象。`printkit <command> --help` 是当前这份构建最权威的选项清单。

`print --batch` 把所有输入当作一个任务：顶层 JSON 数组展开为每个元素一个文档，其余多个输入合并进单个 spooler 任务。单个 PDF 即使带 `--batch` 也原字节直通，不做拼接；不使用 `--batch` 时只接受恰好一个输入文件。显式 `--pdf` 文件排在 `-i` 输入之前，两两各自按出现顺序。

输入文件不存在、`--pdf` 指向的不是 PDF、文件没有 `%PDF-` 头，或 PDF 无法解析，都会在预留任务前以 `E_SCHEMA`（退出码 2）失败。加密 PDF 仍报 `E_PDF_ENCRYPTED`；结构不受支持的 PDF 仍报 `E_UNSUPPORTED`。

`raw` 从不重新编码字节。文件可以是文本（ESC/POS、TSPL、ZPL、CPCL）或二进制；内容原样交给队列。如果打印机驱动拒绝 RAW，命令以 `E_RAW_UNSUPPORTED` 失败，而不是回退到图形打印。

## 全局选项

| 选项 | 含义 |
|---|---|
| `--data-dir DIR` | 数据目录；优先于 `PRINTKIT_DATA_DIR` 与平台默认值 |
| `--port PORT` | 本地服务的回环端口；优先于 `PRINTKIT_PORT` 与 `config.toml` |
| `--log LEVEL` | `off`、`debug`、`info` 或 `usage-only`；优先于 `PRINTKIT_LOG` 与 `config.toml` |
| `-h`, `--help` | 命令帮助；退出码 0 |
| `-V`, `--version` | 二进制版本；退出码 0 |

## 配置

先解析 `data_dir`，再读取 `<data_dir>/config.toml`，每个键独立解析。优先级：命令行选项、环境变量、`config.toml`、服务数据库中存储的值、内置默认值。`data_dir` 不能在 `config.toml` 中设置；允许的 Origin 通过 `POST`/`DELETE /admin/origins` 或 `printkit origin` 管理，不走配置。

`printkit config --json` 列出每个键的生效值和胜出的来源，因此它是当前这份构建最权威的键清单。`--show-source` 为文本输出加注来源，`--path` 只打印文件路径，`--validate` 在键非法时以非零状态退出。`serve` 只在启动时读取一次配置，运行期间不再重读；改动后需重启生效。

环境变量：`PRINTKIT_DATA_DIR`、`PRINTKIT_PORT`、`PRINTKIT_LOG`、`PRINTKIT_RENDER_WORKERS`、`PRINTKIT_QUEUE_MAX`、`PRINTKIT_QUEUE_TIMEOUT_MS`、`PRINTKIT_FETCH_TIMEOUT_MS`、`PRINTKIT_FETCH_MAX_BYTES`。

## 数据目录与便携模式

没有任何覆盖时，macOS 使用 `~/Library/Application Support/printkit/`。该目录包含：

```text
config.toml       optional overrides
printkit.db       SQLite：使用记录、Origin
printkit.db.bak-before-v8   删除 template 表前的备份；可读的旧备份保留，空/损坏的会被替换
printkit.db.bak-before-v9   删除 calibration 表前的备份（仅当表中有数据；可读保留，空/损坏替换）
plugins.toml      内置插件状态（默认 preset core）
plugins.toml.bak* 每次保存或 core 迁移前的备份
plugins.lock      服务生命周期内独占 plugins.toml 的锁
logs/             JSONL 跟踪事件，按留存天数轮转
exports/          `doc.exportPdf` 写出的文件
serve.lock        拥有该数据目录的进程 pid 与端口
admin-token       管理员密钥，仅属主可读（0600）
```

`serve.lock` 保证一个数据目录只有一个服务。对同一目录启动第二个 `serve`，会以「another printkit serve owns ...」退出，并报出 pid 与端口。不要把两个进程指向同一目录，也不要同时运行 `serve` 和会占用该数据目录的 `preview`。

便携模式把一切放在二进制旁边：在可执行文件旁创建一个名为 `portable.flag` 的空文件，数据目录就变成 `<install dir>/data/`。

```sh
touch portable.flag
./printkit config --json     # data_dir points at <install dir>/data
```

## `serve`：端点与鉴权

`printkit serve` 只绑定 `127.0.0.1`（默认端口 `19080`），不提供 HTML。每个响应都是 JSON、WebSocket 帧或二进制下载。桌面应用是 UI；外部集成直接与这些端点通信。

管理员鉴权分三步：

1. `GET /admin/session`（不需要会话）签发 `pk_csrf` cookie，并报告当前是否已登录。
2. `POST /admin/login` 带 `{"token":"<admin-token contents>"}`、`pk_csrf` cookie 与
   `X-CSRF-Token` 头，且 `Origin` 必须指向本服务，成功后设置 `pk_admin` cookie。
3. 每个改变状态的 `/admin` 请求都要回传同一个 `pk_csrf` 值到 `X-CSRF-Token`，并带
   `Origin` 与 `pk_admin` cookie。

令牌文件在首次启动时创建，仅属主可读（Windows 上为仅用户 DACL）。从空 cookie
jar 开始的可执行示例：

```sh
base=http://127.0.0.1:19080
jar=$(mktemp)
curl -s -c "$jar" -o /dev/null "$base/admin/session"
csrf=$(awk '$6=="pk_csrf"{print $7}' "$jar")
curl -s -b "$jar" -c "$jar" -H "Origin: $base" -H "X-CSRF-Token: $csrf" \
  -H 'Content-Type: application/json' \
  -d "{\"token\":\"$(cat "$HOME/Library/Application Support/printkit/admin-token")\"}" \
  "$base/admin/login"
# 此时 pk_admin 已写入 $jar；之后每个写请求都带 Origin 与 CSRF：
curl -s -b "$jar" -H "Origin: $base" -H "X-CSRF-Token: $csrf" \
  -H 'Content-Type: application/json' -d '{"origin":"http://localhost:5173"}' \
  "$base/admin/origins"
```

| 方法 | 路径 | 鉴权 | 用途 |
|---|---|---|---|
| POST | `/admin/login` | admin token + CSRF + Origin | 开始管理员会话 |
| GET | `/admin/session` | 无；签发 `pk_csrf` | 当前会话状态 |
| POST | `/admin/logout` | 管理员会话 + CSRF + Origin | 结束会话并清除 `pk_admin` cookie |
| GET, POST, DELETE | `/admin/origins` | admin | 列出、添加或删除允许的浏览器 Origin（`DELETE` 体为 `{"origin"}`） |
| POST | `/admin/tokens` | admin | 签发绑定到白名单 Origin 的令牌；可选的 `raw` 与 `admin` scope |
| DELETE | `/admin/tokens/{tokenId}` | 管理员会话 + CSRF + Origin | 按 id 撤销令牌；未知或已撤销返回 `E_NOT_FOUND` |
| POST | `/admin/preview` | admin | 为文档打开预览窗口 |
| GET, POST | `/admin/plugins` | admin | 插件状态与配置（core 页） |
| GET | `/admin/logs/events` | admin | 跟踪事件（`observability` 插件） |
| POST | `/admin/logs/export` | admin | 导出诊断包（`observability` 插件） |
| POST | `/admin/shutdown` | admin | 停止服务 |
| GET | `/records/list` | session | 使用记录（`records` 插件） |
| GET | `/records/stats` | session | 聚合使用统计（`records` 插件） |
| GET | `/records/summary` | session | 记录计数与汇总（`records` 插件） |
| GET | `/records/export.csv` | session | 记录导出为 CSV（`records` 插件） |
| GET, HEAD | `/exports/{path}?exp&sig` | loopback `Host` + 签名链接 | 从 `<data_dir>/exports/` 只读访问文件 |
| POST | `/print/batch` | Origin 白名单；令牌可选 | 脚本/非浏览器 `multipart/mixed` 批量（文档或成品 PDF）；浏览器用 WS `doc.printBatch` 打文档、`doc.printPdfBatch` 打成品 PDF |
| GET (upgrade) | `/print` | Origin 白名单；令牌可选 | WebSocket 打印协议 |

去重是尽力而为：WebSocket 的 `requestId` 与 `POST /print/batch` 的 `Idempotency-Key`
在单个服务进程内最多记住 600 秒、1024 条、约 1 MiB。预算内重复调用返回原作业；
超过窗口、被逐出或服务重启后，同一键会再次打印，因此不能重复纸张的调用方需自备业务 id。

打印不需要令牌：白名单内的 `Origin` 和没有 `Origin` 的本地脚本都获得打印 scope。`raw` 与 `admin` 需要经 `/admin/tokens` 签发的令牌；浏览器令牌绑定其确切的 origin。方法清单与负载见 [docs/web-integration.zh-CN.md](web-integration.zh-CN.md)。

## 插件

`records`（`/records/*`、`printkit stats`）与 `observability`（`/admin/logs/*`、
`printkit logs`）是两个内置可选插件。CLI 或 `serve` 的数据目录为 `core` preset，
两者默认关闭，对应路由返回 404；`printkit plugin enable records` 才会打开。桌面
应用首次自建启动时两个插件均已启用，且不覆盖已存在的 `plugins.toml`。插件全关时
core 打印、预览、导出与管理面仍可用。插件状态存于
`<data_dir>/plugins.toml`，升级保留；关闭保留数据，重新开启恢复页面。

`/exports/<path>` 存在的原因是浏览器页面无法导航到 `file://`。它只接受回环
`Host` 头，规范化路径，拒绝 `<data_dir>/exports/` 之外的任何内容。
`doc.exportPdf` 返回签名 `url` 与 `expiresAt`（10 分钟 capability）；链接携带
对规范化文件路径与该过期时间的 per-run HMAC。未签名、已过期或与文件不匹配的
链接返回 `403 E_AUTH`；签名本身就是 capability，因此不要构造、记录或分享裸
`/exports/<path>` URL。`diagnostics.export_ttl_hours` 作用于诊断包目录
（`POST /admin/logs/export` 时清理）以及 `exports/`：每次 `doc.exportPdf` 在写入
产物前删除超过 TTL 的旧文件。成功时返回二进制 PDF/`inline`；该路由的
400/403/404/500 统一为 `application/json; charset=utf-8` 的
`{ok:false,error:{code,message}}`。

## 示例

只渲染，不用打印机，也不用服务：

```sh
./printkit pdf -i document.json -o out.pdf --json
./printkit pdf -i document.json -o out.pdf --dump-html /tmp/document.html
```

打印到指定打印机，再确认队列认识它：

```sh
./printkit printers
./printkit papers -p "Office Printer"
./printkit print -i document.json --printer "Office Printer" -n 2
./printkit print -i part-a.json -i part-b.json --batch --json
./printkit print --pdf existing.pdf --printer "Office Printer"  # 原字节直通
./printkit print -i existing.pdf --json          # 按 .pdf 名称自动识别，原字节直通
./printkit origin add https://app.example
./printkit origin list --json
```

发送设备原始字节、预览，并读取诊断信息：

```sh
./printkit raw -f sample.tspl -p "Office Printer" --title "bin 42"
printf '\x1b@hello\n\x1dV\x00' | ./printkit raw -f - -p "Receipt"
./printkit preview -i document.json
./printkit doctor --json
./printkit logs --stage render --field html_ms --json
./printkit logs --follow
./printkit stats --group printer --from 2026-11-01 --to 2026-11-08 --json
./printkit config --json
./printkit schema --path
```

`printkit logs` 需要 `observability` 插件，`printkit stats` 需要 `records` 插件。默认
`core` preset 下这两条命令会直接报出要启用的插件名；先启动 `serve`，再运行
`printkit plugin enable observability` 或 `records`。

## 退出行为

| 退出码 | 含义 |
|---|---|
| 0 | 成功 |
| 1 | 用法或配置错误（`E_CONFIG`） |
| 2 | 输入或文档未通过 schema 校验（`E_SCHEMA`） |
| 3 | 任务失败：渲染、提交、取消、找不到打印机、超时 |
| 4 | 主机无法完成：渲染器不可用、请求不受支持（`E_UNSUPPORTED`） |

使用 `--json` 时，失败会向 **stderr** 写一个对象，stdout 留给命令自身的输出：

```json
{"code":"E_SCHEMA","message":"...","detail":{"pointer":"/pages/0"}}
```

## 排错

**`schema` 报错「no schema directory beside ...」** — schema 压缩包没有解压到二进制旁边。把 `printkit-schema-*.tar.gz` 解包到与 `printkit` 相同的目录，或运行 `./printkit schema --out DIR`。

**`serve` 提示另一个进程占用了数据目录** — 从消息里或 `<data_dir>/serve.lock` 读取 pid 与端口。用 `--data-dir` 启动第二个实例，或停止另一个进程。

**`logs` 报「observability is disabled」** — 在运行中的服务上执行
`printkit plugin enable observability`；在此之前命令会非零退出，而不是返回空日志。

**`stats` 报「records is disabled」** — 同样执行 `printkit plugin enable records`。
core 打印不依赖这两个插件。

**`E_PRINTER_NOT_FOUND`** — 名称必须与 `printkit printers` 报告的队列一致，包括大小写。省略 `-p` 使用系统默认打印机。

**双面请求报 `E_UNSUPPORTED`** — 在两个当前平台上，双面都会被拒绝，直到物理双面设备经过验证，绝不会悄悄按单面打印。请单面打印，或在打印机驱动里设置双面。

**`resolver busy` / `data.url` 持续 `E_TIMEOUT`** — DNS 解析运行在 4 个固定 worker 上。如果系统解析器一直不返回，这些 worker 无法自行恢复；重启服务（或桌面应用）以获得新的解析池。

**0 页 PDF / macOS 发现** — 0 页 PDF（单份或批量）返回 `E_SCHEMA`；macOS `printers` 只列出可提交的 cupsd 队列，不把 PrintCore-only 名称当作可提交打印机。

**`E_CANCEL_UNSUPPORTED`** — 任务从未获得 spooler id，因此没有可撤回的东西。在 Windows 上，`doc.printPdf` 与 `doc.printBatch` 走 WebView2 PDF 查看器，无法取消。
