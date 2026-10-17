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

## 安全与已知取舍

两轮独立安全审查交叉验证共修 29 处（symlink 沙箱逃逸、CSRF 门、会话完整性、无界读等），全部带回归测试。故意不做（单用户本地场景不值得）：内核会话在内存（重启丢失）、无 SSE 流式、无鉴权/速率限制（`/hook` 与 `/api/run` 无鉴权，公网多用户前请置认证层）、HTTP 不处理 chunked。

上游：[HKUDS/nanobot](https://github.com/HKUDS/nanobot)（Python）。本仓库为独立重写，实现范围聚焦单用户 agent loop + 多渠道网关，不含上游的多 worker/SQLite/六渠道全量。


