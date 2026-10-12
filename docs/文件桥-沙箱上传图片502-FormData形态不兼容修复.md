# 文件桥-沙箱上传图片502-FormData形态不兼容修复

## 问题背景

**现象**：沙箱内 OC 智能体执行 `wecom-cli media upload --json '{"file_path": ".../rabcat.jpg", "type": "image"}'`，稳定返回 502：

```json
{
  "error": {
    "type": "HTTPError",
    "code": 893102,
    "message": "HTTP request failed with status: 502 Bad Gateway",
    "endpoint": "http://127.0.0.1:4096/oc_adapter/ipass-proxy/wecom",
    "status": 502
  }
}
```

排除项（取证数据）：

- 极小文件（78 字节 tiny.png）同样 502 → 与文件体积无关
- `sessions list`、`identity/whoami` 等纯 JSON 请求全通 → OC 隧道与脚本本身正常
- CLI 版本 1.2.0（commit 1b145dd，2026-09-24 构建，即 1.2.0-custom.3，含文件桥代码）
- `--dry-run` 显示 multipart 构造正常；错误 endpoint 是 `/ipass-proxy/wecom`，说明 CLI 侧 `/oss/upload` 签名与 OSS PUT 均已成功

**关键取证**：沙箱 SLS 日志（sandbox-runtime logstore，`proxy.failed` 事件）里的真实错误：

```
IPAAS App工具调用失败，source.on is not a function，工具「企业微信」
```

params 里的 bodyBase64 解码即为规范描述符：`{"file_path":{"url":"https://ai-power-dev.oss-cn-hangzhou.aliyuncs.com/.../rabcat.jpg","filename":"rabcat.jpg","content_type":"application/octet-stream"},"type":"image"}` —— CLI 侧链路完全正常，错误发生在 iPaaS 脚本执行内部。

## 根因

iPaaS expert Action 运行时的全局 `FormData` 是 **npm `form-data` 包**，不是 undici 原生 FormData：

- 佐证一：飞书 `ipaas_action.js`（同运行时、已验证可用）用的是 `form.append(key, file.buffer, {filename, contentType, knownLength})` —— 这是 form-data 包独有的 API 形态
- 佐证二：本地复现（`npm install form-data` 后）：

```
form.append('media', new File([bytes], 'a.jpg'))   → TypeError: source.on is not a function
form.append('media', Buffer.from(bytes), {filename, contentType, knownLength}) → OK
```

form-data 包收到 File/Blob 对象会按流处理，对象上没有 `.on` → 抛 `source.on is not a function`。企微桥接脚本 `makeWecomBridgeFile` 恰恰按 undici 原生形态构造 `new File(...)` 再 append，于是每次上传必炸：脚本抛错 → iPaaS 工具调用失败 → OC adapter（`routes/oc.ts` 的 `ipassProxy`）统一回 502。

## 改动

| 文件 | 改动 |
| --- | --- |
| `docs/wecom-raw-proxy-file-bridge.js` | `buildWecomFileBridgeFormData` 改为飞书同款 `form.append(field, Buffer.from(bytes), {filename, contentType, knownLength})`；删除整个 `makeWecomBridgeFile`（File/Blob 构造逻辑） |
| `docs/wecom-raw-proxy-file-bridge.test.mjs | 新增 form-data 包形态的 `FormDataMock` 替代原生 FormData 全局（mock 对 File/Blob append 直接抛同款错误，作回归守卫）；上传桥断言改为 Buffer + options 透传校验 |

关键代码（替换后）：

```js
form.append(part.name, Buffer.from(part.bytes), {
  filename: part.fileName,
  contentType: part.contentType,
  knownLength: part.bytes.byteLength,
});
```

注意：**CLI、npm 包、Runtime 镜像、OC 路由都不用动**——四层链路验证全部正常，出问题的只有 iPaaS 连接器里粘贴的这份脚本。

## 修复后行为对照

| 场景 | 修复前 | 修复后 |
| --- | --- | --- |
| 纯 JSON 透传 | 正常 | 不变（回归用例覆盖） |
| multipart 上传桥 | 脚本内抛 `source.on is not a function` → 沙箱看到 502 | 描述符 → Buffer append → 企微网关收到真实 multipart |
| 下载桥 / 853004 刷新 / header 黑名单 | 正常 | 不变（用例覆盖） |
| 本地测试 | — | 30/30 通过 |

## 遗留边界

- 脚本头注释里「新增：文件 URL 只允许 https + 域名后缀白名单」一段与决策记录（已移除 host 校验）不一致，属历史残留文案，未在本次修改范围内
- wecom/dingtalk 两个 CLI 的发布脚本仍不注入版本号（dws 自报 `dev`、wecom 自报 `1.2.0` 缺 `-custom.3` 后缀），后续发版时补 `BUILD_SUFFIX` / ldflags `-X`
- 修复需人工把更新后的脚本粘贴到 iPaaS 连接器专家模式并发布新版本后，沙箱重测上传/下载闭环

## 方法论沉淀

- 502 从 OC adapter 返回时，真实错误在沙箱日志 `proxy.failed` 事件（`aipower` service）或 502 响应体 `msg` 字段里，CLI 侧看不到——先查日志再定位，不要在 CLI 侧反复试错
- 写 iPaaS expert 脚本时，全局对象形态以飞书脚本（唯一线上验证过的实现）为准：`FormData` = form-data 包（Buffer + options），不是 undici 原生（File/Blob）
