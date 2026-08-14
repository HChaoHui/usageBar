# usageBar

> AI 订阅用量聚合器 — 常驻 macOS 菜单栏

`usageBar` 把 codex (via CPA)、MiniMax、OpenCode Go 等 AI 编程订阅的剩余配额聚合展示在 macOS 菜单栏，单击图标下拉面板一眼看完。

## 当前状态

- [x] **Phase 1**：Tauri 2 + Rust 脚手架，菜单栏 tray icon + popup 窗口，Linux 开发、macOS 运行
- [x] **Phase 2**：Provider trait + 配置持久化（JSON）
- [x] **Phase 3**：popup UI 完整渲染（进度条、刷新、设置入口、增删 Provider）
- [x] **Phase 4**：真实 Provider 接入
  - ✅ `MinimaxProvider`（订阅 Key 调官方余额接口）
  - ✅ `CpaDirectProvider`（CLIProxyAPI `api-call` 直读 Codex 配额）
  - ✅ `DeepSeekProvider`（官方 API 余额查询，支持 CNY / USD）
  - ✅ `CpaKeeperProvider`（[CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) + [CPA Usage Keeper](https://github.com/Willxup/cpa-usage-keeper)）
  - ✅ `HttpProvider` 通用型（任意 JSON 余额接口 + JSONPath）
  - ✅ `ManualProvider` 手动型
- [x] **Phase 5**：后台定时刷新 + 失败容错（tokio 调度 + cache + 事件推送）
- [x] **Phase 6**：构建 macOS .app 与分发文档

## 已实现功能

- ✅ Tray 图标（左键切换 popup，右键菜单：显示/刷新/退出）
- ✅ Popup 面板：每个订阅一行，进度条 + 剩余百分比 + 剩余/总额 + 错误状态
- ✅ 点击 manual Provider 行 → 弹窗更新「已使用」数量
- ✅ 设置面板：新增和编辑 MiniMax、CLIProxyAPI、DeepSeek Provider，删除 Provider，调整刷新间隔
- ✅ 后台 scheduler：每 N 秒拉取一次（默认 300 秒），结果写入 cache
- ✅ 实时推送：scheduler 完成后向前端发 `usagebar-updated` 事件，前端自动重渲染
- ✅ 配置持久化：JSON 存到 `~/Library/Application Support/com.usagebar.desktop/config.json`（macOS）

## 项目结构

```
usageBar/
├── src/                    # 前端（vanilla HTML/CSS/JS，无构建步骤）
│   ├── index.html
│   ├── styles.css
│   └── main.js
├── src-tauri/              # Rust 后端
│   ├── src/
│   │   ├── main.rs         # 入口（调 lib::run）
│   │   ├── lib.rs          # 应用逻辑、tray、commands、scheduler
│   │   ├── config.rs       # JSON 配置持久化
│   │   └── providers/      # Provider 实现
│   │       ├── mod.rs      # Provider trait + build_provider 工厂
│   │       ├── manual.rs   # 手动型
│   │       ├── http.rs     # 通用 HTTP + JSONPath 型
│   │       ├── minimax.rs  # MiniMax 订阅 Key 型
│   │       ├── cpa_direct.rs  # CLIProxyAPI 直连 Codex 配额
│   │       ├── deepseek.rs    # DeepSeek 官方余额接口
│   │       └── cpa_keeper.rs  # CPA Usage Keeper 适配
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── capabilities/
│   │   └── default.json    # Tauri 权限
│   └── icons/
├── config.example.json     # 配置示例（含 MiniMax / CPA / OpenCode Go 模板）
└── README.md
```

## 配置 Provider

应用首次启动后 config.json 不存在，UI 弹出「设置」即可添加。

### manual — 手动维护

适合没有 API 的服务（如 OpenCode Go）。填总额度，已使用量在 UI 上点击行手动更新。

### http — 通用 HTTP 接口

任何返回 JSON 的余额查询接口都能用：

| 字段 | 必填 | 说明 |
|---|---|---|
| `endpoint` | ✅ | API 地址（GET 请求） |
| `api_key` |  | Bearer Token（部分服务需要） |
| `json_used` | ✅ | 「已使用」字段的 JSON 路径（如 `data.credits.used`） |
| `json_total` | ✅ | 「总额度」字段的 JSON 路径 |
| `json_unit` |  | 单位字段路径（可选，默认用 UI 上填的） |
| `timeout_secs` |  | 超时秒数（默认 15） |

JSON 路径支持点号嵌套：`data.balance.used`。数字/字符串数字都能解析。

### minimax — MiniMax 订阅 Key

```json
{
  "type": "minimax",
  "endpoint": "https://www.minimaxi.com/v1/token_plan/remains",
  "api_key": "<订阅 Key>",
  "unit": "%"
}
```

调 `GET https://www.minimaxi.com/v1/token_plan/remains`，读取并同时展示 `general` 模型的 5 小时和每周窗口，以及各自的剩余百分比和重置时间。`endpoint` 可替换为账号所在区域的 MiniMax API 地址。

### cpa_direct — CLIProxyAPI 直连 Codex

CLIProxyAPI 7.2.111 可通过 `POST /v0/management/api-call` 代理访问 Codex 官方 usage 接口，因此不安装 Keeper 也能读取当前配额：

```json
{
  "type": "cpa_direct",
  "endpoint": "http://127.0.0.1:8317",
  "api_key": "<CPA 管理密钥>",
  "auth_index": "0123456789abcdef",
  "account_id": "00000000-0000-4000-8000-000000000000",
  "quota_window": "auto",
  "unit": "%"
}
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `endpoint` | ✅ | CLIProxyAPI 根地址，通常是 `http://127.0.0.1:8317` |
| `api_key` | ✅ | `remote-management.secret-key` 对应的原始管理密码，不是 `api-keys` |
| `auth_index` | ✅ | `GET /v0/management/auth-files` 返回的 Codex `auth_index` |
| `account_id` |  | ChatGPT Account ID；当前 usage 接口可不填，必要时从 auth-files 的 `id_token.chatgpt_account_id` 获取 |
| `quota_window` | ✅ | `auto` / `five_hour` / `weekly` / `monthly` |

`auto` 会在服务端实际返回的窗口中选择已用百分比更高的一项。窗口类型根据 `limit_window_seconds` 判断，而不是根据 `primary_window` 或 `secondary_window` 判断：`18000` 是 5 小时，`604800` 是每周。

管理密钥只用于请求 CLIProxyAPI；转发给 ChatGPT 的 `$TOKEN$` 会由 CPA 按 `auth_index` 自动替换。此管理接口权限很高，建议只允许本机访问。

### deepseek — DeepSeek API 余额

调用官方 `GET https://api.deepseek.com/user/balance`，显示指定币种的总余额、充值余额、赠送余额以及 API 是否可用。

```json
{
  "type": "deepseek",
  "endpoint": "https://api.deepseek.com/user/balance",
  "api_key": "<DeepSeek API Key>",
  "currency": "CNY"
}
```

`currency` 支持 `CNY` 和 `USD`。若接口没有返回所选币种，应用会显示第一条余额记录。

### cpa_keeper — CPA Usage Keeper（codex / claude / gemini 等）

[CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) 自 v6.10.0 移除了内置持久化用量面板。若需要持久化、历史统计和多服务统一配额缓存，可安装 [CPA Usage Keeper](https://github.com/Willxup/cpa-usage-keeper)。本应用调用它当前的 `POST /api/v1/quota/cache` 接口读取指定账号的 `usedPercent`。

```json
{
  "type": "cpa_keeper",
  "endpoint": "http://127.0.0.1:8080",
  "path": "/api/v1/quota/cache",
  "auth_index": "0123456789abcdef",
  "row_key": "rate_limit.primary_window",
  "unit": "%"
}
```

`auth_index` 必填，它不是账号序号或邮箱。可从 Keeper 的 `GET /api/v1/usage/identities` 返回值中的 `identity` 获取，也可从 CLIProxyAPI 的 `GET /v0/management/auth-files` 返回值中的 `auth_index` 获取。

常用 `row_key`：

| 服务 | row_key | 含义 |
|---|---|---|
| codex | `rate_limit.primary_window` | Primary 位置，实际周期看 Keeper 的 `window.seconds` |
| codex | `rate_limit.secondary_window` | Secondary 位置，实际周期看 Keeper 的 `window.seconds` |
| claude | `five_hour` | 5h 用量 |
| claude | `seven_day` | 周用量 |
| gemini_cli | `gemini_cli.quota.buckets[0].remainingFraction` | ⚠ 需反推 |
| antigravity | 同 gemini_cli |

Keeper 若启用了 `AUTH_ENABLED`，先登录 `/api/v1/auth/login`，再把完整的 `cpa_usage_keeper_session=...` cookie 填到 `api_key`。这里不能填写 CPA 管理密码。本机使用建议 Keeper 监听 `127.0.0.1` 并设置 `AUTH_ENABLED=false`。

### 配置示例

参见 `config.example.json`，含 MiniMax、CLIProxyAPI 直连、CPA Keeper 和 OpenCode Go 配置：

```bash
# macOS 默认配置位置
~/Library/Application Support/com.usagebar.desktop/config.json

# Linux（开发时）
系统配置目录下的 com.usagebar.desktop/config.json
```

UI 上添加的 provider 会自动持久化，无需手动编辑 JSON。

## 安全与隐私

- usageBar 不包含遥测、分析或崩溃上报，也不会把配置发送给项目维护者。
- API Key、管理密钥和 Session Cookie 仅用于请求用户配置的 Provider 地址。
- 凭据保存在本机应用配置目录的 `config.json` 中；Unix 系统会将文件权限限制为仅当前用户可读写。
- 设置页读取配置时不会把已保存的凭据返回给 WebView。编辑 Provider 时将凭据输入框留空即可保留原值。
- 修改 Provider 的 API 地址时必须重新输入凭据，携带凭据的请求不会自动跟随 HTTP 重定向。
- 携带凭据的远程接口必须使用 HTTPS；HTTP 只允许访问 `localhost`、`127.0.0.1` 或 `::1`。
- 不要上传真实 `config.json`、接口响应、HAR 文件、日志、证书或签名材料。

安全问题请按 [SECURITY.md](SECURITY.md) 中的方式私下报告。

## 开发

### 前置依赖（Ubuntu 24.04）

```bash
sudo apt-get install -y libwebkit2gtk-4.1-dev libxdo-dev libssl-dev \
  libayatana-appindicator3-dev librsvg2-dev patchelf build-essential
```

Rust 工具链：

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
cargo install tauri-cli --version "^2.0" --locked
cargo install create-tauri-app --locked
```

### 开发模式（带热重载）

```bash
cargo tauri dev
```

> Linux 上跑 `cargo tauri dev` 需要图形会话（X11/Wayland），无 GUI 环境只能用 `cargo check` / `cargo build` 做编译验证。

### 仅编译验证

```bash
cd src-tauri && cargo check              # 快速类型检查（~1 秒）
cd src-tauri && cargo build --release    # 完整 release 构建（首次 ~3 分钟）
```

### 文件位置速查

| 文件 | 作用 |
|---|---|
| `src-tauri/src/lib.rs` | Tauri commands、tray 逻辑、scheduler |
| `src-tauri/src/providers/mod.rs` | Provider trait + `build_provider()` 工厂 |
| `src-tauri/src/providers/http.rs` | HTTP/JSON 实现 |
| `src-tauri/src/config.rs` | JSON 配置读写 |
| `src/main.js` | 前端所有逻辑（render、settings、edit modal） |
| `src/styles.css` | popup 样式（macOS-like blur + dark/light auto） |

## 构建 macOS .app

> 在 Linux 上交叉编译 .app 是可行的，但代码签名需要 macOS。

### 方案 A：在 Mac 上构建（推荐）

把项目复制到 MacBook Air M2，安装上述依赖（外加 Xcode Command Line Tools：`xcode-select --install`）后：

```bash
cd src-tauri
cargo tauri build --target aarch64-apple-darwin
# 产物：src-tauri/target/aarch64-apple-darwin/release/bundle/macos/usageBar.app
# DMG（可选）：src-tauri/target/aarch64-apple-darwin/release/bundle/dmg/usageBar_0.1.0_aarch64.dmg
```

把 `usageBar.app` 拖入 `/Applications`。首次启动：
- **右键 → 打开**（绕过 Gatekeeper，仅个人使用无需开发者签名）
- 系统设置 → 通用 → 登录项 → 勾选 `usageBar` 启用开机启动
- 状态栏右上角出现 tray icon（系统偏好设置 → 菜单栏可隐藏本机图标）

### 方案 B：Linux 交叉编译（无签名）

需要 macOS SDK + osxcross，环境搭建复杂（要先 dump macOS SDK，编译 osxcross 工具链），仅在无法访问 Mac 时考虑。**个人使用强烈建议用方案 A**。

```bash
# 这条命令会失败，因为缺少 macOS 工具链
rustup target add aarch64-apple-darwin
cd src-tauri
cargo tauri build --target aarch64-apple-darwin
# 实际部署：在 Mac 上执行 `cargo tauri build`
```

## 技术栈

- **Tauri 2**：Rust 后端 + 系统 WebView，体积小（约 10 MB）、启动快
- **vanilla HTML/CSS/JS**：前端零构建步骤，菜单栏面板本就简单
- **tokio**：异步运行时（Provider 拉取 + scheduler）
- **reqwest + rustls**：HTTPS 请求（不依赖系统 OpenSSL）
- **serde / serde_json**：序列化 + JSON 路径解析
- **async-trait**：Provider 接口抽象

## License

[MIT](LICENSE) © 2026 HuangChaoHui
