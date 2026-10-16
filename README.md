# nanobot-rs — nanobot (HKUDS) 重写 polyglot 产物

原 Python 框架的各模块按「最适合语言」拆散重写，四台件各自独立可跑：

## 组件

| 组件 | 语言 | 位置 | 职责 |
|---|---|---|---|
| **agent 内核 + HTTP API** | Rust (stdlib + ureq/serde_json) | `src/main.rs` | system prompt（AGENTS/SOUL/USER.md + memory）、7 工具、OpenAI 兼容 tool_call 循环、tiny HTTP 服务 |
| **渠道网关** | Go（零依赖） | `gateway/` | Telegram 长轮询、cron（`NANOBOT_CRON="HH:MM prompt|..."`）、`/hook` 通用 webhook、反代 `/api/run`、伺服 WebUI |
| **WebUI** | 原生 HTML/JS | `gateway/webui/index.html` | 单文件聊天页 |
| **MCP 适配器** | Node JS（零 npm 依赖） | `mcp/nanobot-mcp.js` | MCP stdio JSON-RPC，tools/list / tools/call → core `/api/tool` |

## 运行

```sh
# 内核 API（必需）
NANOBOT_SERVE=8090 NANOBOT_API_BASE=... NANOBOT_API_KEY=... NANOBOT_MODEL=... ./target2/release/nanobot-rs
# CLI 交互模式则不设 NANOBOT_SERVE

# 网关（可选）
(cd gateway && NANOBOT_CORE=http://127.0.0.1:8090 NANOBOT_PORT=8081 TELEGRAM_BOT_TOKEN=... NANOBOT_CRON="09:00 daily brief" ./gateway-bin)

# MCP（接 Claude/任何 MCP 客户端）
NANOBOT_CORE=http://127.0.0.1:8090 node mcp/nanobot-mcp.js
```

## 端点

core: `GET /health`（精确匹配）· `POST /api/run {prompt, session_id?}`（in-memory 会话，200 条截断）· `POST /api/tool {name, arguments}`（非 JSON body → 400）
gateway: `GET /`（WebUI）· `GET /api/health`（反代 core 健康检查，core 挂时 502 JSON）· `POST /api/run`（反代，**必须 application/json**，无 session_id 时自动分配私有会话）· `POST /hook {text, session_id?}`（同上）

## 环境变量汇总

| 变量 | 组件 | 默认 | 说明 |
|---|---|---|---|
| `NANOBOT_API_BASE` / `NANOBOT_API_KEY` / `NANOBOT_MODEL` / `NANOBOT_WORKSPACE` | core | OpenAI 公共端点 / 必填 / gpt-4o-mini / `.nanobot` | LLM 与沙箱根 |
| `NANOBOT_SERVE` | core | 不设 = CLI 模式 | 设成端口号进 HTTP 模式（只绑 127.0.0.1） |
| `NANOBOT_CORE` | gateway / mcp | `http://127.0.0.1:8090` | 内核地址 |
| `NANOBOT_BIND` / `NANOBOT_PORT` | gateway | `127.0.0.1` / `8081` | **默认只绑回环**；`/hook` 与 `/api/run` 无鉴权，暴露公网前先加认证层 |
| `TELEGRAM_BOT_TOKEN` | gateway | 空 = 不启 TG | Telegram 长轮询 |
| `NANOBOT_TG_ALLOW` | gateway | 空 = 全放行 | 逗号分隔的 chat id 白名单，建议生产必设 |
| `NANOBOT_CRON` | gateway | 空 = 不调度 | `"HH:MM prompt\|HH:MM prompt"`，每日触发；超过计划点 30 分钟内会补发一次（晚发好过不发） |
| `NANOBOT_TG_OFFSET_FILE` | gateway | `tg-offset.txt` | Telegram 轮询 offset 持久化文件，避免重启重放 24h 旧消息 |
| `NANOBOT_EXEC_TIMEOUT` | core | `60` | `exec` 工具的单命令墙钟上限（秒）；超时杀进程，输出超 8MB 也会中止 |

## 测试与验证

- `cargo test --release`：**11/11**（路径逃逸+符号链接逃逸、edit 唯一匹配、exec 沙箱+60s 超时、grep、大文件截断、UTF-8 边界截断、小写 content-length、头洪泛 400 行上限、单行 8KB 增量上限）。
- `go test`：atoi 边界（空串/非数字）；`go vet` 干净。
- `test_all.sh`：mock OpenAI 拉起 core+gateway+MCP 全链路，**14 条真实断言**（health、/api/health、hook、webui 反代、415 双门、fuzz 路由 404、bad JSON 400、MCP initialize/call/unknown-isError/-32601/-32700），全绿才退出 0。
- WebUI 真机浏览器实测：core 挂 → `core offline`（502 不误亮）；复活 → 下个 10s 周期翻回 `core online`；收发气泡 + bot 回复 `tool said: …` 全通。

## 审查记录（2026-10-17，全量复审）

**第一轮已修**：
1. **WebUI 完全不可用（高）** —— 页面 fetch `/health` 打到网关（无该路由）→ 404 非 JSON → 状态永远 `core offline` → 发送被前置拦截。修：网关新增 `/api/health` 反代 + 前端改相对路径。浏览器实测前后对比确认。
2. **`/api/health` 失败分支 content-type 错误（中）** —— `http.Error` 发 text/plain，前端 `r.json()` 必炸。修：显式 JSON 头 + 502。
3. **HTTP 头解析两处（中）** —— `h[..15]` 字节切片在多字节首部处可 panic；头行数无上限可被慢速客户端拖住连接。修：整行 lowercase + 200 行上限，补 flood 回归测试（并修了测试自身不关客户端句柄导致的假挂起）。
4. **`tgSend` 字节截断（中）** —— `text[:4000]` 会把多字节 UTF-8 切碎成无效文本。修：按 rune 截断到 3900 + 标记。
5. **网关默认监听 0.0.0.0（中）** —— 无鉴权的 `/hook`、`/api/run` 直接暴露。修：默认 `127.0.0.1`，`NANOBOT_BIND` 可覆盖。
6. **TG 无白名单（中）** —— 任何找到 bot 的人都能烧你的 API key。修：`NANOBOT_TG_ALLOW`，非白名单 chat 记日志丢弃。
7. **`/hook` body 无上限（低）**、`coreRun` 循环内 defer（低）、cron 非法时分静默（低）、MCP `grep.path`/`list_dir.path` 误标 required（低）、MCP 死代码（低）、`test_all.sh` 的 `head -4` SIGPIPE 误杀 node（低）—— 全部修。

### 第二轮复审（同日晚：两个独立零上下文审查员 + 39 条动态探针实测，不带此前结论）

**已修（新增 18 处）**：
- **[高] workspace 符号链接逃逸（rustc 实验证实）**：workspace 内 planted symlink 可将 write_file 引出根。修：resolve() 对存在组件逐级 canonicalize，最终目标必须 stays-inside，新增回归测试。
- **[高] `/api/run` 不检 Content-Type（实测证实面）**：`text/plain` 是 CORS 简单请求——任意浏览中的网页可直接 POST 驱动 agent（通过 `exec` 工具）= CSRF RCE。修：应用级 JSON 门（415），与 `/hook` 一致。
- **[高] 会话毒化**：chat 出错/32 轮打满时 `msgs.last()` 是 role:"tool"，落库后下次回放 API 400 且会话永久损坏（审查员静态推导 + 代码路径确认）。修：仅持久化纯 assistant 文本回复，淘汰整对裁切；CLI transcript 同规则（200 条尾）。
- **[高] WebUI 健康探测状态漏洞 + 自己写出的真回归**：原探测不看 `r.ok`（502 JSON 也亮 online）；修补过程中 Node 单例复现发现 `then(r=>r.json()).then(j=>{…r.ok…})` 里第二个回调引用 `r` 是自由变量 → ReferenceError 被自身 catch 吞掉 → 页面**恒 offline**。终版：`r.ok` 检查前置 + 502/`j.ok` 双检 + 10s 周期复检，四分支 Node 单测 + 浏览器真机全通。
- **[中] core HTTP 资源防线（实测证实）**：慢速客户端钉线程 30s+；单行 header 先整行缓冲（实测 3MB 过线）再拒绝。修：socket 30s read timeout + `read_line_capped()` 增量逐段 8KB 封顶，双回归测试。
- **[中] 路由前缀过松（实测证实）**：`/healthzzz` 200、`/api/toolzzz` 真执行命令。修：去 query + 精确匹配 + fuzz 断言入集成套件。
- **[中] cron 语义三连（Go 实测证实 `atoi("")`==0）**：`"0: prompt"` 静默排进 00:00；错过分钟整日不发；id 无分隔可碰撞、标记泄漏。修：atoi 空串=-1（附带 Go 单测）；30 分钟内晚发补发；`HH:MM-key` 分隔 + 按天重置。
- **[中] 网关外呼零超时**：core 挂起永久钉 handler，goroutine 无限累积。修：coreRun 320s / `/api/health` 10s 专用客户端。
- **[中] TG offset 只在内存**：重启重放 24h 旧消息（重复烧 LLM）。修：`NANOBOT_TG_OFFSET_FILE` 落盘 + `allowed_updates` QueryEscape。
- **[中] coreRun 重试非幂等**：响应中断后重发会二次执行 agent / 双倍副作用。修：仅 dial 阶段错误（`net.OpError.Op=="dial"`）才重试。
- **[中] exec 工具无超时无输出上限**：`sleep 999999` 钉死 agent、`yes` 灌爆内存。修：`run_with_timeout()`（默认 60s，`NANOBOT_EXEC_TIMEOUT` 可调）+ 双管道并发排水 + 8MB 输出帽 + 超时/超帽分文案 + 信号击杀不再谎报 exit 0。
- **[中] 文件工具无界读**：read_file/edit_file/naive_grep 全量 load 大文件。修：`read_file_capped()` 8MB 截断带标记；edit_file 拒绝超大文件并指路 exec。
- **[中] 7 处 `http.Error` text/plain 装 JSON** + core 端点对非 JSON body 静默当 `{}`。修：jsonErr/writeText 统一真 JSON；core 显式 400 `bad json body`。
- **[中] rg 零命中回退全扫** + naive_grep 语义漂移。修：rg 成功即返回（含零命中），仅 rg 不可用时回退。
- **[中] MCP**：fetch 120s AbortController；core 错误补 `isError:true`（原先伪装成成功输出）；stdin 无换行洪泛 10MB 自杀；畸形行回 `-32700` 而非静默。
- **[低] 匿名会话串线**：缺省 `web`/`hook` 会话所有匿名调用方共享 → 按次生成私有 id（cron 保留固定会话是有意设计）。
- **[低] 其余**：`{"text":%q}` 可能产出非 JSON → `json.Marshal`；WebUI 路径 CWD 相对 → 按可执行文件解析；`find_files` 达帽即停；`..` 整组件判定（`a..b.txt` 合法）；session_store 更名去谎；启动日志 typo；**test_all.sh 重写为 14 条真实断言**（旧版只打印不断言，错误码/坏 JSON 也报「全绿」——重写当轮即抓出 3 个真实回归）。

**复审后终态**：`cargo test --release` 11/11 · clippy 0 warning · `go vet`/`go test` 干净 · `node --check` OK · `test_all.sh` 14/14 PASS · 浏览器端到端 offline→online→收发全链路通过。

**明知而未做（记录在案）**：内核会话仍在内存（重启丢失，多 worker 不共享）；无 SSE 流式；无鉴权/速率限制（已有 415 门 + 1MB body 上限，但公网多用户仍应前置认证层）；HTTP 不处理 chunked；core 每连接一线程无总数帽。这五条只在公网多租户场景下才值得做。

