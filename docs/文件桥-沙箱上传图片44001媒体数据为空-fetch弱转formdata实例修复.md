# 文件桥-沙箱上传图片44001媒体数据为空-fetch弱转formdata实例修复

## 问题背景

**现象**：上一轮 502 修复（`source.on is not a function`，见《文件桥-沙箱上传图片502-FormData形态不兼容修复.md》）发布 V2 后，上传仍然失败，但错误变了：

```json
{
  "error": {
    "code": 44001,
    "message": "媒体数据为空 (callid: 1791613386393612153199280)"
  }
}
```

（会话 ses_edb9928b6ffeFusUSrC3pFeSUx，2026-10-10 06:23 UTC，jpg/txt/大小文件共 11 次尝试全部 44001）

排除项（取证数据）：

- 沙箱 SLS `proxy.failed` 06:23 之后零命中 → iPaaS 执行器没有抛错，脚本逻辑跑通了
- 窗口内无 893102 / 502 → 不再是 OC 适配器 502
- 各类文件（145KB jpg、6 字节 txt）同样 44001 → 与文件内容/体积无关
- 每个 CLI 调用 ~2s 内返回 → OSS 上传、脚本下载、企微调用全链路都执行了

**结论性证据**（本地复现，`/tmp/formdata-repro2`，form-data@4.0.6）：

| fetch 实现 | form 实例直接作 body | 结果 |
| --- | --- | --- |
| node-fetch@2.7.0 | ✅ 正常 | content-type 带 boundary，316 字节完整 multipart |
| 原生 undici fetch（Node 26） | ❌ **损坏** | body 变成 17 字节字符串 `"[object FormData]"`，content-type 变 `text/plain;charset=UTF-8` |

## 根因

上一轮修复只解决了 **append 形态**（`Buffer + {filename, contentType, knownLength}`），但把 form-data 实例直接交给了 fetch。iPaaS Action 运行时的 fetch 是 undici 形态：不认识 npm form-data 包实例，把它当作普通对象做 WebIDL 字符串弱转 → 企微端收到的是一段 `[object FormData]` 文本，根本不是 multipart → 解析不出媒体字段 → `44001 媒体数据为空`。

（注：node-fetch@2 恰好支持 form-data 包实例，所以「飞书脚本这么写能用」的经验在本运行时不可迁移——飞书脚本 `ipaas_action.js` 把 form 实例直接传给 fetch 的写法，在该运行时同样存在此隐患，需另行核实飞书侧实际上传是否可用。）

## 改动

`docs/wecom-raw-proxy-file-bridge.js` 两处：

1. `buildWecomFileBridgeFormData`：不再返回 form 实例，改为**自己序列化**——`form.getBuffer()` 产出完整 multipart 字节，`form.getHeaders()` 给出含 boundary 的 content-type，返回 `{__wecomBridgeMultipart: true, contentType, buffer}`。boundary 每次重建独立生成，与当次 buffer 配套。
2. `sendWecomRawRequest`：识别 `__wecomBridgeMultipart` 标记，把当次序列化的 boundary 写入请求头 `content-type`，body 用 Buffer（fetch 对 Buffer body 的处理是通用的，任何 fetch 实现都安全）。

测试 `docs/wecom-raw-proxy-file-bridge.test.mjs`：

- FormDataMock 补齐 `getBuffer()`/`getHeaders()`（按 form-data 包语义最小实现）
- 上传桥断言改为：fetch 收到的 body 必须是**已序列化的 Buffer**（非 form-data 实例）、multipart 字节含 boundary/filename/Content-Type/文件字节、请求头 content-type 带当次 boundary、旧 boundary（STALE）不透传
- 回归守卫注释里写明 44001 根因

验证：`node docs/wecom-raw-proxy-file-bridge.test.mjs` → 27/27 通过。

## 修复后行为对照

| 维度 | 修复前（V2） | 修复后 |
| --- | --- | --- |
| fetch 收到的 body | form-data 实例 → 弱转 `"[object FormData]"` 17 字节文本 | 完整 multipart Buffer |
| 请求 content-type | text/plain;charset=UTF-8（undici 自动判定） | multipart/form-data; boundary=…（getHeaders 提供） |
| 企微响应 | 44001 媒体数据为空 | 预期返回 media_id（待沙箱实测） |
| 853004 重放 | — | 每次重放重建 form → 新 boundary 与新 buffer 配套，无跨次污染 |

## 遗留边界

- `getBuffer()` 会把整个 multipart 物化进内存：上传内存峰值 ≈ 2× 文件体积（下载缓存 + 序列化副本）。文件桥上限 100MB 时峰值 ~200MB，若后续放开大文件需改流式（`form.getStream()` + transfer-encoding，但依赖 fetch 支持流 body，需先验证运行时）。
- 企微 CLI 网关对 /cli/media/upload 的 multipart 字段名以方法元数据为准（实际观察为 `file_path` + `type`，来自 get_cli_config 的工具定义），脚本按描述符键原样透传，不做字段名改写。
- 飞书 `ipaas_action.js` 同样把 form 实例直接交给 fetch——若飞书走同一运行时且实际用过文件上传，需核实是否同样损坏；若从未真实验证过，属同源隐患。
- 待办：沙箱实测上传 + 下载（`__ipaas_file` envelope）双路径 e2e。

## 方法论沉淀

- **「上游验证可用」的写法要连运行时一起验证**：append 形态（form-data 包）和 body 序列化（fetch 实现）是两个独立变量，只验证一个不够。
- 判别技巧：服务端报「媒体数据为空 / body 为空」类错误 + 客户端无异常 → 优先怀疑传输层弱转（toString/stringify 把二进制 body 变文本）。
- 本地复现两连招：抓原始 HTTP（http.createServer 收 raw bytes + headers）对比不同 fetch 实现的输出，一次就能把「弱转」钉死。
