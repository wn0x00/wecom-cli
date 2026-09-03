# 企微 CLI 接入 OpenCode iPaaS Proxy 方案

## 目标

`wecom-cli` 只运行在 OpenCode 的会话 Sandbox 内。所有企业微信 API 请求都经 OpenCode 转发到唯一的 `wecom` iPaaS executor；CLI 不直接访问企业微信网关，也不在本地维护企业微信 OAuth/token 生命周期。

```text
wecom-cli
  -> POST ${WECOM_CLI_OC_ADAPTER_URL}/ipass-proxy/wecom
  -> OpenCode iPaaS proxy route
  -> wecom iPaaS executor
  -> 企业微信 API
```

`WECOM_CLI_OC_ADAPTER_URL` 仅用于发现同一会话中的 OpenCode 地址，不控制是否启用代理。代理行为是 CLI 的固定行为；OpenCode 通过 shell 环境注入该地址，方式与飞书 CLI 一致。

## 当前差异

当前 `custom-endpoint` 只会替换企业微信网关的 base URL，不能连接 OpenCode proxy：

- CLI 会请求 `/service/discovery`、schema 和实际企业微信 API path；OpenCode 的入口是固定的 `/oc_adapter/ipass-proxy/wecom`。
- CLI 将多数请求编码为 `{"payload":"<json>"}`，而 OpenCode 路由要求 `{ method, path, query, headers, body }`。
- CLI 的 `WecomBackend` 会要求本地 Bearer token，并在 `853004` 时刷新；代理模式中授权属于 iPaaS auth-check 和 OpenCode interrupt。
- CLI 的响应 envelope 默认期待企业微信网关的 `{ errcode, errmsg, results_json }`，需与 iPaaS executor 返回值保持一致。

## 固定代理契约

CLI 对每一个原始企业微信 HTTP 请求创建如下 OC 请求：

```http
POST ${WECOM_CLI_OC_ADAPTER_URL}/ipass-proxy/wecom
content-type: application/json
x-ipass-session-id: ${IPASS_SESSION_ID}
x-ipass-call-id: ${IPASS_ACTIVE_CALL_ID}
x-ipass-message-id: ${IPASS_ACTIVE_MESSAGE_ID}
```

```json
{
  "method": "POST",
  "path": "/service/discovery",
  "query": "",
  "headers": { "x-wecom-cli-info": "..." },
  "body": {}
}
```

- `method`、`path`、`query`、`headers`、`body` 来自 CLI 即将发送的原始企业微信请求。
- `path` 保留企业微信 API path，不能替换为 OC path；OC 使用固定 HTTP URL 接收请求，iPaaS 根据 body 中的原 path 发起真正的 SaaS 调用。
- `Authorization`、bot secret、CLI 本地 token 不得进入 `headers`。
- `IPASS_ACTIVE_CALL_ID` 和 `IPASS_ACTIVE_MESSAGE_ID` 缺失时不发送对应 header；PTY 等非工具调用场景保持可用，但不能将授权卡回挂到特定工具 part。

## CLI 改造

### 1. 取代当前网关 transport

在 `crates/wecom-cli/src/transport/` 新增 OC proxy backend。它在实际 reqwest 出网前截获 endpoint、HTTP method、query、headers 和 JSON body，并调用固定 OC URL。

不再以 `WECOM_CLI_BASE_URL` 替换企业微信网关。`custom-endpoint` 及其 `WECOM_CLI_ACCESS_TOKEN` / `WECOM_CLI_AUTH_ENDPOINT` 不承担生产代理职责。

### 2. 保留 CLI 的本地能力，移除本地 SaaS 鉴权

保留 discovery 缓存、动态 schema、命令解析、参数组装、文件系统与输出路由。它们仍以企业微信 API path 为输入。

proxy backend 不使用 `WecomBackend` 的以下逻辑：

- `RequireAuth` 本地门禁；
- Bearer token 注入；
- botid/secret 换 token；
- `853004` 自动刷新和重放。

iPaaS connector 的 auth-check 不通过时，由 OpenCode interrupt 请求授权；授权完成后恢复同一次 CLI 请求。

### 3. 响应契约

第一期让 `wecom` iPaaS executor 返回企业微信 CLI 已有的网关响应：

```json
{
  "errcode": 0,
  "errmsg": "ok",
  "results_json": "{\"result\":\"...\"}"
}
```

这样 CLI 可继续使用 `NestedRes`，无需同时改动 CLI 输出、分页和长任务解析。iPaaS 错误同样映射成该 envelope 的非零 `errcode`。

若 connector 无法稳定提供此格式，再新增只在 proxy backend 使用的 OC response envelope；不要修改通用 transport 默认 envelope。

### 4. 文件与长任务范围

第一期只验收 JSON 请求/响应，包括 discovery、schema 和普通服务方法。

- multipart 上传、媒体下载、octet-stream 和断点续传暂不进入首批范围；OC 现有 proxy body 是 JSON，无法直接承载这些载荷。
- 长任务仅在 iPaaS 返回现有 `taskid` / polling 语义时启用；否则先禁止对应命令，待协议扩展后再支持。

## OpenCode 配套改动

企微 CLI 完成后，在 OpenCode 仓库：

1. `shell.env` 注入 `WECOM_CLI_OC_ADAPTER_URL=${serverOrigin}/oc_adapter`，复用既有 `IPASS_SESSION_ID`、`IPASS_ACTIVE_CALL_ID`、`IPASS_ACTIVE_MESSAGE_ID`。
2. 在 iPaaS proxy config 中添加 `wecom -> connectorCode`，确保只有一个 `wecom` executor。
3. 在 Sandbox 镜像安装企微 CLI，并将企微 skills 的命名空间加入 permission allowlist。
4. 为 `/ipass-proxy/wecom` 增加路由契约测试：headers、原请求映射、授权 interrupt 上下文和响应透传。

## 实施顺序与验收

1. 为 proxy backend 写 mock OC 测试：断言固定 URL、五个 body 字段及三个 iPaaS header。
2. 实现 proxy backend，并让 discovery 请求通过 mock OC 返回预设 catalog。
3. 验证一个普通 schema 方法能完成 CLI -> OC -> mock executor 的完整调用。
4. 在 OpenCode 注册 `wecom` connector，验证未授权时授权卡能关联到 bash 工具调用，授权后可重试成功。
5. 打包到 E2B，运行真实 CLI JSON API 冒烟测试。

验收标准：CLI 进程不直接连接企业微信域名；未授权请求由 OpenCode 弹 iPaaS 授权；每次 JSON API 请求均带会话 header 并由唯一 `wecom` executor 处理；现有飞书链路无回归。
