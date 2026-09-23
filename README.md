# usageBar

> AI 订阅用量聚合器 — 常驻 macOS 菜单栏

`usageBar` 把 codex (via CPA)、MiniMax、OpenCode Go 等 AI 编程订阅的剩余配额聚合展示在 macOS 菜单栏，单击图标下拉面板一眼看完。

## 当前状态

- [x] **Phase 1**：Tauri 2 + Rust 脚手架，菜单栏 tray icon + popup 窗口，Linux 开发、macOS 运行
- [x] **Phase 2**：Provider trait + 配置持久化（JSON）
- [x] **Phase 3**：popup UI 完整渲染（进度条、刷新、设置入口、增删 Provider）
- [x] **Phase 4**：真实 Provider 接入
  - ✅ `MinimaxProvider`（订阅 Key 调官方余额接口）
  - ✅ `McodeProvider`（独立 MiniMax OAuth 登录，显示套餐、Credits 与账号额度）
  - ✅ `ClinePassProvider`（独立 Cline 登录，显示 ClinePass 5 小时 / 每周 / 每月额度）
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
- ✅ 折叠卡片：左侧薄环和充足内径容纳剩余百分比，右侧上下排列其余窗口的横向进度条；底部只显示一条主窗口倒计时（如「5 小时 · 36 分钟后重置」），完整重置日期在悬停提示中查看
- ✅ 展开卡片：恢复经典布局，每个窗口一行「窗口名 + 大号剩余百分比 + 横向进度条 + 相对/精确重置时间」，各条目之间有细分隔线
- ✅ 点击 manual Provider 行 → 弹窗更新「已使用」数量
- ✅ 设置面板：新增 Provider、显示/隐藏 Provider、删除 Provider、调整刷新间隔
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
│   │       ├── mcode.rs    # MiniMax OAuth 登录账号型
│   │       ├── clinepass.rs # ClinePass 订阅额度和套餐
│   │       ├── cpa_direct.rs  # CLIProxyAPI 直连 Codex 配额
│   │       ├── deepseek.rs    # DeepSeek 官方余额接口
│   │       └── cpa_keeper.rs  # CPA Usage Keeper 适配
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── capabilities/
│   │   └── default.json    # Tauri 权限
│   └── icons/
├── config.example.json     # 配置示例（含 MiniMax / ClinePass / CPA / OpenCode Go 模板）
└── README.md
```

## 配置 Provider

应用首次启动后 config.json 不存在，UI 弹出「设置」即可添加。

### 显示与隐藏

设置列表每行左侧的眼睛按钮控制该 Provider 是否在主页显示，对应配置中的 `enabled` 字段：

- 隐藏后主页不再渲染该卡片，后台调度也暂停查询，不消耗额度和网络请求。
- 配置、加密凭证和登录状态都会保留；需要查看时再次点击眼睛按钮，会立即查询并显示。
- 隐藏与显示只影响 `enabled`，不会影响 Provider 顺序、套餐绑定或账号登录。

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

### mcode — MiniMax 独立登录账号

```json
{
  "type": "mcode",
  "display_name": "MiniMax 个人账号",
  "unit": "%"
}
```

不需要填写 API Key 或 Access Token，也不需要安装 MCode。usageBar 独立执行 MiniMax OAuth Device Flow + PKCE S256，调用账号接口，显示：

- Token Plan 套餐等级
- 套餐到期时间
- 剩余 Credits
- 5 小时、每周和视频额度
- 积分签到状态，可领取时手动签到

使用方式：

1. 在设置中添加 `MiniMax（独立登录）` Provider，填写用于区分账号的卡片名称，点击「创建并登录」。
2. 为该卡片选择「国内账号登录」或「国际账号登录」，在 MiniMax 官方网页登录并授权。需要时输入面板中的授权码；也可复制授权地址到浏览器。
3. 授权成功后凭证自动保存，卡片显示该账号的额度、余额及签到。再添加一张卡片即可登录另一个账号，同名卡片也会单独展示。浏览器可能记住上次的账号，授权时可在官方页面切换账号；展开卡片可核对账号名称及 ID。
4. 编辑卡片可以查看登录状态、取消待完成的授权，或退出该卡片的账号后重新登录。退出、签到、凭证刷新只针对当前卡片。删除卡片会清除它的本地凭证；若需撤销官方授权，先点击「退出账号」。

保留配置类型 `mcode` 和原有 Provider ID。旧版共享 OAuth 凭据会自动迁移到配置中第一张 `mcode` 卡片，其余卡片分别登录；不会将同一个 Refresh Token 复制给多个卡片。如果旧版尚无卡片，第一张新建并访问的登录卡片会接收旧凭据。迁移完成后删除旧的共享凭据文件。更早的本地 MCode 登录态仍需在 usageBar 内重新授权。

展开首页的 MiniMax 账户明细可查看紧凑的七日签到栏：七天排成一行，展示积分、额外奖励和领取状态，今天高亮，标题右侧可手动签到。悬停每天可查看完整天数、积分和状态；大额积分以紧凑格式展示。支持浅色和深色主题，数据来自官方响应，不补造缺失数据。面板保留滚轮和触控板滚动，隐藏占用宽度的滚动条，避免内容挤压。

官方返回可领取时显示「签到领积分」，点击后先重新查询资格，再领取奖励并刷新签到状态和 Credits 余额；已签到时显示「今日已签到」。折叠卡片会以「可签到」提示待领取奖励。未订阅 Token Plan 的账号也会查询签到状态；查询失败时显示「签到状态暂不可用」，不影响已有套餐及额度展示。仅在手动点击时领取，不会自动签到。领取请求超时后先刷新官方状态，不自动重复提交。

凭据位于 Tauri 应用数据目录下的 `minimax-oauth/accounts/<Provider ID 的 SHA-256>/`。每张卡片有独立的 `account.enc`（AES-256-GCM 加密）和 `vault.key`，临时文件写入后原子替换；重命名、排序不改变凭据绑定。Unix 下私有目录权限为 `0700`、文件权限为 `0600`；密钥与密文同机保存，保护依赖操作系统账号权限。根目录文件锁防止多个 usageBar 实例同时操作凭据，每个账号有独立请求锁，避免不同账号相互阻塞刷新或登录。重启应用会恢复各卡片的登录；密钥缺失或密文损坏会报错，不会静默覆盖。

Access Token、Refresh Token 和 PKCE verifier 不返回前端，也不写入 `config.json`。每次查询前检查 Token，有效期不足 60 秒时通过 `/oauth2/token` 刷新；账号请求遇到鉴权失败时刷新并重试一次。轮换后的 Refresh Token 立即加密保存。退出会请求 `/oauth2/revoke` 并清除本地凭据，远端撤销失败时会提示。

协议参考 MiniMax Code 客户端，使用 `client_id=mcode-public`、`scope=agent.default`、`audience=agent-backend`，因此官方授权页可能显示 MiniMax Code。usageBar 的登录与本地 MCode 完全独立，不读取其文件或环境变量。该功能依赖 MiniMax 账号接口，协议更新后可能需要适配。

### clinepass — ClinePass 独立登录账号

```json
{
  "type": "clinepass",
  "display_name": "ClinePass",
  "unit": "%"
}
```

不需要填写 API Key。usageBar 独立执行 Cline 的 WorkOS Device Flow OAuth：申请设备授权码，在官方网页登录确认，再把登录结果注册为 Cline API 凭证。每张卡片独立绑定一个 Cline 账号，显示：

- ClinePass 套餐名称
- 当前计费周期结束时间
- 5 小时、每周、每月额度的已用百分比和重置时间

卡片折叠时默认显示 **5 小时**窗口；可在设置的类型字段中选择「折叠显示窗口」切换为每周、每月或「自动（显示最紧张窗口）」。左侧 64px 薄环内显示剩余百分比和“剩余”小字，右侧依次排列其余窗口的横向进度条，标签、轨道和数值按列对齐。下方只显示一条主窗口倒计时，如「5 小时 · 36 分钟后重置」；悬停可查看完整重置日期，不把相对时间与日期硬塞进同一行。未提供重置时间时明确标注，按次数计费的窗口保留剩余量。折叠状态额度区域背景透明，以留白区分；展开后每个窗口各占一行，使用窗口名 + 大号百分比 + 横向进度条 + 重置时间的经典布局，条目之间以细分隔线区分。已有卡片未配置时按 5 小时处理。

使用方式：

1. 在设置中添加 `ClinePass（独立登录）` Provider，填写用于区分账号的卡片名称，点击「创建并登录」。
2. 点击「登录 Cline 账号」，在打开的官方网页完成登录并输入面板中的授权码；也可复制授权地址到浏览器。
3. 面板自动检测授权结果，显示「已连接」后即可看到三个额度窗口。再添加一张卡片即可登录另一个 Cline 账号。
4. 切换账号时点击「退出账号」再重新登录；待完成的授权可以随时取消。

凭证位于 Tauri 应用数据目录下的 `cline-oauth/accounts/<Provider ID 的 SHA-256>/`，与 MiniMax 使用相同的加密与锁策略：每张卡片独立的 `account.enc`（AES-256-GCM）和 `vault.key`，私有目录 `0700`、文件 `0600`，请求和刷新按账号隔离。Cline 的 Access Token 在调用账号接口时以 `Bearer workos:<token>` 发送；有效期不足 60 秒或接口返回 401 时自动刷新。Cline 未提供适合桌面客户端的远程撤销接口，因此「退出账号」只清除本地凭证。

数据来自 Cline 官方接口 `GET /api/v1/users/me/plan/usage-limits`、`/users/me` 和 `/users/me/plan`，与 `app.cline.bot` 订阅页使用相同来源。额度窗口和套餐信息缺失时只影响对应明细，不影响登录。该功能依赖 Cline 的 OAuth 和账号接口，官方调整协议后可能需要适配。

### cpa_direct — CLIProxyAPI 直连 Codex

CLIProxyAPI 可通过 `POST /v0/management/api-call` 代理访问 Codex 官方接口，因此不安装 Keeper 也能读取当前配额、套餐和手动完整重置次数：

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

`auto` 会在服务端实际返回的所有窗口中选择已用百分比更高的一项。窗口类型根据 `limit_window_seconds` 判断，而不是根据 `primary_window` 或 `secondary_window` 判断：`18000` 是 5 小时，`604800` 是每周，28 至 31 天按月度窗口处理。展开后会显示普通 Codex、Code Review 和 `additional_rate_limits` 中的全部额度窗口。

设置页可通过 CPA 的 `GET /v0/management/auth-files` 自动发现 Codex OAuth 账号并填充 `auth_index`、ChatGPT Account ID。首页账户明细会显示 `plan_type`、`available_count`、`applicable_available_count`，并按到期时间逐条列出 `codex_rate_limits` 类型的可用重置额度。订阅到期时间只在 `wham/usage` 明确返回时显示，不采用可能过期的 auth-file Token 元数据。

“使用一次完整重置”会在二次确认后，通过 CPA 代理调用 `POST https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume`。请求使用新的 `redeem_request_id`，操作会真实消耗一次重置额度且不可撤销。CPA 自身的 `POST /v0/management/reset-quota` 只清除本地 cooldown 状态，本应用不会将它误作完整重置。

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
