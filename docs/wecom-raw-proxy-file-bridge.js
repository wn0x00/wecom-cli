/**
 * WeCom CLI raw transport Action for Yingdao iPaaS expert mode.
 * ============================================================
 * 基线：conn_996337955385700352 / wecom_cli_raw_proxy（V1）
 * 本版新增：文件桥（file bridge）
 *   - 上传：multipart 请求体改为 JSON 描述符 → 脚本下载 OSS 文件 → 重组 FormData
 *   - 下载：二进制响应 → 流式上传 iPaaS OSS → 回 {__ipaas_file:{...}} envelope
 *
 * Input (context.params):
 *   { provider, method, path, query, headers, bodyBase64 }
 * Output:
 *   { status, headers, bodyBase64 }
 *
 * 与 CLI 的接口约定（两侧必须一致）
 * ---------------------------------
 * 上传（CLI → 本脚本），信号是 headers["content-type"]：
 *   headers: { "content-type": "multipart/form-data" }        // 不带 boundary，脚本重建
 *   bodyBase64 = base64(JSON.stringify({
 *     "type": "file",                                          // 普通文本字段
 *     "media": { "url": "https://<oss>/...",                   // 文件字段：值含 url + filename
 *                "filename": "a.pdf",
 *                "content_type": "application/pdf" }
 *   }))
 *   判定：值是有 url + filename 的对象 ⇒ 文件；其余 ⇒ 文本字段；键名 = multipart field name
 *
 * 下载（本脚本 → CLI）：把 envelope 放进 bodyBase64，headers 换成 application/json
 *   headers: { "content-type": "application/json" }
 *   bodyBase64 = base64(JSON.stringify({
 *     "__ipaas_file": { "oss_url": "...", "content_type": "...", "filename": "..." }
 *   }))
 *
 * 文件 URL 校验：与飞书对齐，不做 host 校验
 * -------------------------------------
 * 飞书 send_request 的实现是裸 `fetch(desc.url)`，无 host 白名单。本脚本同样不做 host/协议校验，
 * 只保留 redirect:"error" 与单文件体积上限（后者是功能性限制，防 OOM）。
 *
 * 决策记录：最初曾加过 host 白名单（腾讯 COS 上海：cos.ap-shanghai.myqcloud.com）以及
 * 「拒绝内网/回环/链路本地/云元数据」的地址校验，后按「安全防护对齐飞书」的要求移除。
 * 已知残留风险（有意接受，不是遗漏）：URL 由沙箱内可执行任意代码的 agent 控制，
 * 而本 Action 跑在平台网络位，因此存在盲 SSRF——拉到的内容不直接回给调用方，
 * 但错误信息中的 `HTTP <status>` 可作为状态码探针，且内容若形似文件可借道
 * 企微 /file/upload 取得 media_id 后外发。若后续需要收紧，恢复被移除的
 * validateWecomBridgeFileUrl / isWecomBridgeBlockedHost 两个函数即可。
 *
 * 保留的另两项约束（与 SSRF 无关，是功能正确性所需）：
 *   - 桥接只在 WECOM_RAW_PROXY_FILE_BRIDGE_ALLOWED_PATHS 声明的上传路径上生效；
 *   - 文件名必须净化（防路径穿越与非法字符，见 sanitizeWecomBridgeFileName）。
 *
 * Security boundaries（在原版基础上新增）
 * --------------------------------------
 * - 原版全部保留：上游 origin 固定、99 条 path 白名单、双向 header 黑名单、
 *   调用方凭据/cookie/代理头全部丢弃、真实 Bearer 只来自 auth.outputs.token、
 *   853004 时内存内单次刷新 token、不回传任何 token / Bot Secret。
 * - 新增：文件 URL 只允许 https + 域名后缀白名单 + 拒绝内网/回环/链路本地/云元数据地址，
 *   禁止跳转、限制单文件体积与数量；文件 URL 与 OSS 签名不写入日志。
 * - 桥接只在 WECOM_RAW_PROXY_FILE_BRIDGE_ALLOWED_PATHS 声明的上传路径上生效。
 * - 遗留凭证模型不变：本 Action 仍然可以承载写操作，不应暴露为任意网络代理。
 */
async function main({ context }) {
  const WECOM_RAW_PROXY_BASE_URL = "https://qyapi.weixin.qq.com/cli";
  const WECOM_RAW_PROXY_AUTH_ENDPOINT =
    "https://qyapi.weixin.qq.com/cgi-bin/aibot/cli/get_cli_config";
  const WECOM_RAW_PROXY_TOKEN_EXPIRED_CODE = 853004;
  const WECOM_RAW_PROXY_MAX_REQUEST_BYTES = 16 * 1024 * 1024;
  const WECOM_RAW_PROXY_MAX_RESPONSE_BYTES = 64 * 1024 * 1024;
  const WECOM_RAW_PROXY_MAX_QUERY_LENGTH = 8192;
  const WECOM_RAW_PROXY_MAX_INSPECT_BYTES = 1024 * 1024;

  // ── 文件桥配置 ────────────────────────────────────────────

  // 桥接允许生效的上传路径（必须是 ALLOWED_PATHS 的子集）。
  const WECOM_RAW_PROXY_FILE_BRIDGE_ALLOWED_PATHS = new Set([
    "/file/upload",
    "/media/upload",
    "/disk/files/upload",
    "/doc/import",
    "/sheet/import",
    "/smartsheet/import",
    "/smartsheet/files/upload",
    "/smartsheet/images/upload",
    "/smartpage/files/upload",
    "/smartpage/images/upload",
  ]);

  // 单文件上限，与企微 MAX_UPLOAD_SIZE（100MB）对齐；单请求只允许一个文件。
  const WECOM_RAW_PROXY_FILE_BRIDGE_MAX_BYTES = 100 * 1024 * 1024;
  const WECOM_RAW_PROXY_FILE_BRIDGE_MAX_FILES = 1;

  // 响应侧 envelope 键名，与飞书实现保持一致。
  const WECOM_RAW_PROXY_FILE_ENVELOPE_KEY = "__ipaas_file";

  // OSS 中转 key 前缀。
  const WECOM_RAW_PROXY_FILE_OSS_KEY_PREFIX = "/ipaas/wecom/";

  // ── 路径白名单（原版保留，99 条：95 schema + 4 官方内建） ──

  const WECOM_RAW_PROXY_ALLOWED_PATH_LIST = Object.freeze([
    "/file/upload",
    "/remote_doc/get",
    "/service/discovery",
    "/task/query",
    "/calendar/schedules/list", "/calendar/schedules/get",
    "/calendar/schedules/search", "/calendar/schedules/create",
    "/calendar/schedules/update", "/calendar/schedules/cancel",
    "/calendar/schedules/free/list", "/meeting/rooms/buildings/list",
    "/meeting/rooms/search", "/chat/groups/list", "/chat/messages/list",
    "/contact/users/search", "/disk/files/download", "/disk/files/get",
    "/disk/files/list", "/disk/files/rename", "/disk/files/search",
    "/disk/files/upload", "/disk/folders/create", "/doc/create", "/doc/import",
    "/doc/search", "/doc/contents/append", "/doc/contents/get",
    "/doc/contents/overwrite", "/doc/members/update", "/doc/names/update",
    "/doc/rules/update", "/mail/get", "/mail/search", "/mail/send",
    "/media/download", "/media/upload", "/message/send", "/message/aibot/send",
    "/message/aibot/sessions/list", "/message/files/get", "/meeting/cancel",
    "/meeting/create", "/meeting/get", "/meeting/list", "/meeting/search",
    "/meeting/update", "/meeting/original/get", "/sheet/create", "/sheet/get",
    "/sheet/import", "/sheet/contents/update", "/sheet/ranges/get",
    "/sheet/rows/append", "/sheet/subsheets/add", "/sheet/subsheets/delete",
    "/smartpage/create", "/smartpage/import", "/smartpage/blocks/update",
    "/smartpage/databases/get", "/smartpage/files/upload",
    "/smartpage/images/upload", "/smartpage/pages/append",
    "/smartpage/pages/get", "/smartpage/pages/overwrite",
    "/smartpage/pages/update", "/smartsheet/create", "/smartsheet/get",
    "/smartsheet/import", "/smartsheet/charts/add", "/smartsheet/charts/delete",
    "/smartsheet/charts/list", "/smartsheet/charts/update",
    "/smartsheet/fields/add", "/smartsheet/fields/delete",
    "/smartsheet/fields/list", "/smartsheet/fields/update",
    "/smartsheet/files/upload", "/smartsheet/images/upload",
    "/smartsheet/records/add", "/smartsheet/records/delete",
    "/smartsheet/records/list", "/smartsheet/records/query",
    "/smartsheet/records/update", "/smartsheet/sheets/add",
    "/smartsheet/sheets/delete", "/smartsheet/sheets/list",
    "/smartsheet/sheets/update", "/smartsheet/views/add",
    "/smartsheet/views/delete", "/smartsheet/views/list",
    "/smartsheet/views/update", "/todo/create", "/todo/delete", "/todo/finish",
    "/todo/get", "/todo/list", "/todo/update", "/identity/whoami"
  ]);

  const WECOM_RAW_PROXY_ALLOWED_PATHS = new Set(WECOM_RAW_PROXY_ALLOWED_PATH_LIST);
  const WECOM_RAW_PROXY_BLOCKED_REQUEST_HEADERS = new Set([
    "authorization", "connection", "content-length", "cookie", "forwarded", "host",
    "keep-alive", "origin", "proxy-authenticate", "proxy-authorization", "referer",
    "set-cookie", "te", "trailer", "transfer-encoding", "upgrade", "via",
    "x-http-method-override", "x-method-override", "x-original-url", "x-real-ip",
    "x-rewrite-url", "xybot-product"
  ]);
  const WECOM_RAW_PROXY_BLOCKED_RESPONSE_HEADERS = new Set([
    "connection", "content-encoding", "content-length", "keep-alive",
    "proxy-authenticate", "proxy-authorization", "set-cookie", "set-cookie2", "te",
    "trailer", "transfer-encoding", "upgrade"
  ]);

  // ── 主流程 ────────────────────────────────────────────────

  async function executeWecomRawProxy(input, context, runtime) {
    const safeRuntime = runtime || {};
    validateProvider(input && input.provider);
    const method = validateProxyMethod(input && input.method);
    const path = validateProxyPath(input && input.path);
    const query = normalizeProxyQuery(input && input.query);
    const requestHeaders = normalizeProxyRequestHeaders(input && input.headers);
    const requestBody = decodeProxyBase64(
      input && input.bodyBase64,
      WECOM_RAW_PROXY_MAX_REQUEST_BYTES,
      "bodyBase64",
      safeRuntime
    );
    const initialToken = readProxySessionToken(context);
    const request = {
      method,
      url: `${WECOM_RAW_PROXY_BASE_URL}${path}${query}`,
      headers: requestHeaders,
      body: requestBody
    };

    // 文件桥（上传）：multipart 请求体不是字节，而是一份 JSON 描述符。
    const bridge = await buildWecomFileBridgeRequest(path, requestHeaders, requestBody, safeRuntime);
    if (bridge) {
      request.headers = bridge.headers;
      // 每次发送/重放都重新构建 FormData（文件字节已缓存），避免 body 被消费后无法复用。
      request.body = () => buildWecomFileBridgeFormData(bridge, safeRuntime);
    }

    const first = await sendWecomRawRequest(request, initialToken, safeRuntime);
    if (inspectWecomErrorCode(first.body) !== WECOM_RAW_PROXY_TOKEN_EXPIRED_CODE) {
      return serializeRawProxyResponse(first, safeRuntime);
    }

    const refreshedToken = await refreshProxyBearerToken(context, safeRuntime);
    const second = await sendWecomRawRequest(request, refreshedToken, safeRuntime);
    return serializeRawProxyResponse(second, safeRuntime);
  }

  // ── 校验与归一化（原版保留） ──────────────────────────────

  function validateProvider(value) {
    if (value == null || String(value).trim() === "") return;
    if (String(value).trim().toLowerCase() !== "wecom") {
      throw new Error("provider 仅允许 wecom");
    }
  }

  function validateProxyMethod(value) {
    const method = String(value == null ? "POST" : value).trim().toUpperCase();
    if (method !== "POST") {
      throw new Error("method 仅允许企业微信 CLI 使用的 POST");
    }
    return method;
  }

  function validateProxyPath(value) {
    const path = String(value == null ? "" : value).trim();
    if (!WECOM_RAW_PROXY_ALLOWED_PATHS.has(path)) {
      throw new Error(
        `path 必须是 ${WECOM_RAW_PROXY_ALLOWED_PATH_LIST.length} 条企业微信 CLI 固定路径之一`
      );
    }
    return path;
  }

  function normalizeProxyQuery(value) {
    if (value == null || value === "") return "";
    let query;
    if (typeof value === "string") {
      query = value.startsWith("?") ? value.slice(1) : value;
      if (query.includes("#") || /[\u0000-\u001f\u007f]/.test(query)) {
        throw new Error("query 包含不允许的字符");
      }
    } else {
      const source = parsePlainJsonObject(value, "query");
      const pairs = [];
      for (const key of Object.keys(source)) {
        if (!key || /[\u0000-\u001f\u007f]/.test(key)) {
          throw new Error("query 参数名不合法");
        }
        const values = Array.isArray(source[key]) ? source[key] : [source[key]];
        for (const item of values) {
          if (item == null) continue;
          if (!["string", "number", "boolean"].includes(typeof item)) {
            throw new Error(`query 参数 ${key} 仅支持字符串、数字、布尔值或其数组`);
          }
          pairs.push(`${encodeURIComponent(key)}=${encodeURIComponent(String(item))}`);
        }
      }
      query = pairs.join("&");
    }
    if (query.length > WECOM_RAW_PROXY_MAX_QUERY_LENGTH) {
      throw new Error(`query 不能超过 ${WECOM_RAW_PROXY_MAX_QUERY_LENGTH} 个字符`);
    }
    return query ? `?${query}` : "";
  }

  function normalizeProxyRequestHeaders(value) {
    const source = value == null || value === "" ? {} : parsePlainJsonObject(value, "headers");
    const headers = {};
    for (const originalName of Object.keys(source)) {
      const name = String(originalName).trim().toLowerCase();
      if (!isValidHeaderName(name)) throw new Error(`headers 包含不合法字段名: ${originalName}`);
      if (isBlockedProxyRequestHeader(name)) continue;
      const rawValue = Array.isArray(source[originalName])
        ? source[originalName].map(item => String(item)).join(", ")
        : String(source[originalName] == null ? "" : source[originalName]);
      if (/\r|\n/.test(rawValue)) throw new Error(`header ${name} 包含换行符`);
      if (rawValue.length > 8192) throw new Error(`header ${name} 超过 8192 个字符`);
      headers[name] = rawValue;
    }
    return headers;
  }

  function isBlockedProxyRequestHeader(name) {
    return WECOM_RAW_PROXY_BLOCKED_REQUEST_HEADERS.has(name) ||
      name.startsWith("x-forwarded-") ||
      name.startsWith("x-ipass-") ||
      name.startsWith("x-yingdao-");
  }

  function isValidHeaderName(value) {
    return /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/.test(value);
  }

  function parsePlainJsonObject(value, name) {
    let parsed = value;
    if (typeof value === "string") {
      try {
        parsed = JSON.parse(value);
      } catch (_) {
        throw new Error(`${name} 必须是合法 JSON 对象`);
      }
    }
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
      throw new Error(`${name} 必须是 JSON 对象`);
    }
    return parsed;
  }

  // ── 文件桥（上传）：描述符 → OSS 下载 → FormData ──────────

  async function buildWecomFileBridgeRequest(path, headers, bodyBytes, runtime) {
    const contentType = String(headers["content-type"] || "").toLowerCase();
    if (!contentType.startsWith("multipart/form-data")) return null;

    if (!WECOM_RAW_PROXY_FILE_BRIDGE_ALLOWED_PATHS.has(path)) {
      throw new Error(`path ${path} 不在文件桥允许的上传路径白名单内`);
    }

    let descriptor;
    try {
      descriptor = JSON.parse(decodeProxyUtf8(bodyBytes) || "{}");
    } catch (_) {
      throw new Error("multipart 桥接请求体必须是 JSON 描述符");
    }
    if (!descriptor || typeof descriptor !== "object" || Array.isArray(descriptor)) {
      throw new Error("multipart 桥接请求体必须是 JSON 对象");
    }

    const parts = [];
    let fileCount = 0;
    for (const key of Object.keys(descriptor)) {
      const value = descriptor[key];
      if (value === undefined || value === null) continue;
      const values = Array.isArray(value) ? value : [value];
      for (const item of values) {
        if (item === undefined || item === null) continue;
        if (isWecomFileDescriptor(item)) {
          fileCount += 1;
          if (fileCount > WECOM_RAW_PROXY_FILE_BRIDGE_MAX_FILES) {
            throw new Error(
              `单次 multipart 上传只允许 ${WECOM_RAW_PROXY_FILE_BRIDGE_MAX_FILES} 个文件字段`
            );
          }
          parts.push({ kind: "file", name: key, desc: item });
          continue;
        }
        if (typeof item === "object") {
          parts.push({ kind: "text", name: key, value: JSON.stringify(item) });
          continue;
        }
        parts.push({ kind: "text", name: key, value: String(item) });
      }
    }

    if (fileCount === 0) {
      throw new Error("multipart 桥接请求体未包含文件描述符");
    }

    for (const part of parts) {
      if (part.kind !== "file") continue;
      const downloaded = await downloadWecomBridgeFile(part.desc, runtime);
      part.bytes = downloaded.bytes;
      part.fileName = downloaded.fileName;
      part.contentType = downloaded.contentType;
    }

    // boundary 由运行时按新的 FormData 生成，原 content-type 不复用。
    const nextHeaders = Object.assign({}, headers);
    delete nextHeaders["content-type"];

    return { headers: nextHeaders, parts };
  }

  function isWecomFileDescriptor(value) {
    return Boolean(
      value &&
      typeof value === "object" &&
      !Array.isArray(value) &&
      typeof value.url === "string" &&
      value.url.trim() !== "" &&
      typeof value.filename === "string" &&
      value.filename.trim() !== ""
    );
  }

  function buildWecomFileBridgeFormData(bridge, runtime) {
    if (typeof FormData !== "function") {
      throw new Error("当前 Action 运行时不支持 FormData");
    }
    const form = new FormData();
    let fileIndex = 0;
    for (const part of bridge.parts) {
      if (part.kind === "text") {
        form.append(part.name, part.value);
        continue;
      }
      fileIndex += 1;
      // 运行时的 FormData 是 npm form-data 包，只认
      // append(field, Buffer, {filename, contentType, knownLength})；
      // 传 File/Blob 会抛 "source.on is not a function"。
      form.append(part.name, Buffer.from(part.bytes), {
        filename: part.fileName,
        contentType: part.contentType,
        knownLength: part.bytes.byteLength,
      });
    }
    if (fileIndex === 0) {
      throw new Error("multipart 桥接未准备任何文件字段");
    }
    // 不把 form-data 实例直接交给 fetch：运行时 fetch（undici 形态）会把
    // 陌生 body 对象弱转成 "[object FormData]" 文本 + text/plain，
    // 企微端收不到 multipart → 44001 媒体数据为空。
    // 自己序列化：getBuffer() 产出完整 multipart 字节，getHeaders() 给出含
    // boundary 的 content-type（boundary 每次重建独立，与当次 buffer 配套）。
    return {
      __wecomBridgeMultipart: true,
      contentType: form.getHeaders()["content-type"],
      buffer: form.getBuffer(),
    };
  }

  async function downloadWecomBridgeFile(desc, runtime) {
    // 与飞书对齐：不校验 host / 协议（见文件头「决策记录」）。
    const fetchImpl = resolveProxyFetch(runtime);
    let response;
    try {
      response = await fetchImpl(desc.url, { method: "GET", redirect: "error" });
    } catch (_) {
      throw new Error("下载中转文件失败：无法连接文件地址");
    }
    if (!response.ok) {
      throw new Error(`下载中转文件失败: HTTP ${response.status}`);
    }

    const declaredLength = Number(response.headers.get("content-length"));
    if (Number.isFinite(declaredLength) && declaredLength > WECOM_RAW_PROXY_FILE_BRIDGE_MAX_BYTES) {
      throw new Error(
        `中转文件超过 ${WECOM_RAW_PROXY_FILE_BRIDGE_MAX_BYTES} 字节上限`
      );
    }

    let arrayBuffer;
    try {
      arrayBuffer = await response.arrayBuffer();
    } catch (_) {
      throw new Error("下载中转文件失败：响应读取失败");
    }
    const bytes = new Uint8Array(arrayBuffer);
    if (bytes.byteLength > WECOM_RAW_PROXY_FILE_BRIDGE_MAX_BYTES) {
      throw new Error(`中转文件超过 ${WECOM_RAW_PROXY_FILE_BRIDGE_MAX_BYTES} 字节上限`);
    }

    const fileName = sanitizeWecomBridgeFileName(
      desc.filename,
      response.headers.get("content-disposition")
    );
    const contentType =
      typeof desc.content_type === "string" && desc.content_type.trim()
        ? desc.content_type.trim()
        : (response.headers.get("content-type") || "application/octet-stream");

    return { bytes, fileName, contentType };
  }

  // 文件名净化：防目录穿越与非法字符。
  // 背景：影刀 RPA 机器人用 Content-Disposition 命名本地文件，且不解析 filename*=UTF-8''，
  // 会生成含 `*` 的非法 Windows 文件名导致下载失败。
  function sanitizeWecomBridgeFileName(rawName, disposition) {
    let name = typeof rawName === "string" ? rawName : "";
    if (!name && disposition) name = parseWecomDispositionFilename(disposition);
    const cleaned = String(name || "")
      .split(/[\\/]/)
      .pop()
      .replace(/[\u0000-\u001f<>:"|?*]/g, "_")
      .trim()
      .slice(0, 120);
    return cleaned || `wecom-download-${Date.now()}.bin`;
  }

  function parseWecomDispositionFilename(disposition) {
    const encoded = /filename\*=UTF-8''([^;]+)/i.exec(disposition);
    if (encoded && encoded[1]) {
      try {
        return decodeURIComponent(encoded[1]);
      } catch (_) {
        return encoded[1];
      }
    }
    const plain =
      /filename="([^"]+)"/i.exec(disposition) || /filename=([^;]+)/i.exec(disposition);
    return plain && plain[1] ? plain[1].trim() : "";
  }

  // ── 文件桥（下载）：二进制 → 流式上传 OSS → envelope ───────

  // 判定：content-type 非 JSON / 非 text/* ⇒ 二进制。
  function isWecomBinaryResponse(response) {
    const contentType = (response.headers.get("content-type") || "").toLowerCase();
    if (!contentType) return false;
    if (contentType.includes("application/json") || contentType.includes("text/json")) return false;
    if (contentType.startsWith("text/")) return false;
    return true;
  }

  async function buildWecomBinaryEnvelope(response, runtime) {
    const ReadableImpl = resolveProxyReadable(runtime);
    const uploadImpl = resolveProxyFileUpload(runtime);
    const contentType = response.headers.get("content-type") || "application/octet-stream";
    const fileName = sanitizeWecomBridgeFileName(
      "",
      response.headers.get("content-disposition")
    );
    const uniqueSuffix = `${Date.now()}_${Math.random().toString(36).slice(2, 10)}`;
    const ossKey = `${WECOM_RAW_PROXY_FILE_OSS_KEY_PREFIX}${uniqueSuffix}_${fileName}`;

    if (!response.body) throw new Error("企业微信 CLI 响应体为空，无法上传到 OSS");

    const stream = ReadableImpl.from(response.body);
    const url = await uploadImpl(stream, ossKey, fileName);
    if (typeof url !== "string" || !url.trim()) {
      throw new Error("iPaaSFileUpload 未返回可用链接");
    }

    return {
      [WECOM_RAW_PROXY_FILE_ENVELOPE_KEY]: {
        oss_url: url,
        content_type: contentType,
        filename: fileName
      }
    };
  }

  function resolveProxyReadable(runtime) {
    const impl = (runtime && runtime.Readable) || (typeof Readable !== "undefined" ? Readable : undefined);
    if (!impl || typeof impl.from !== "function") {
      throw new Error("当前 Action 运行时不支持 Readable.from，无法流式上传到 OSS");
    }
    return impl;
  }

  function resolveProxyFileUpload(runtime) {
    const impl =
      (runtime && runtime.iPaaSFileUpload) ||
      (typeof iPaaSFileUpload !== "undefined" ? iPaaSFileUpload : undefined);
    if (typeof impl !== "function") {
      throw new Error("当前 Action 运行时不支持 iPaaSFileUpload，无法中转二进制响应");
    }
    return impl;
  }

  // ── 鉴权与请求发送（原版保留，仅扩展 body 形态） ──────────

  function readProxySessionToken(context) {
    const auth = context && context.property && context.property.auth;
    const outputs = (auth && auth.outputs) || {};
    const token = String(outputs.token == null ? "" : outputs.token).trim();
    if (!token) throw new Error("当前影刀授权输出缺少 token");
    if (token.length > 4096 || /[\u0000-\u001f\u007f]/.test(token)) {
      throw new Error("当前影刀授权输出 token 不合法");
    }
    return token;
  }

  function readProxyBotCredentials(context) {
    const auth = context && context.property && context.property.auth;
    const inputs = (auth && (auth.inputs || auth.params)) || {};
    return {
      botId: requiredProxyString(inputs.bot_id, "bot_id"),
      secret: requiredProxyString(inputs.bot_secret, "bot_secret")
    };
  }

  async function sendWecomRawRequest(request, token, runtime) {
    const fetchImpl = resolveProxyFetch(runtime);
    const headers = Object.assign({}, request.headers, { authorization: `Bearer ${token}` });
    const body = typeof request.body === "function" ? request.body() : request.body;
    let bodyBytes = body;
    if (body && body.__wecomBridgeMultipart) {
      // multipart 桥：把当次序列化得到的 boundary 写进请求头，
      // body 用已序列化的字节（fetch 对 Buffer 的处理是通用的）。
      headers["content-type"] = body.contentType;
      bodyBytes = body.buffer;
    }
    let response;
    try {
      response = await fetchImpl(request.url, {
        method: request.method,
        headers,
        body: prepareProxyRequestBody(bodyBytes),
        redirect: "error"
      });
    } catch (_) {
      throw new Error("企业微信 CLI 官方网关请求失败");
    }
    return captureRawProxyResponse(response, runtime);
  }

  function prepareProxyRequestBody(body) {
    if (!body) return undefined;
    if (typeof FormData === "function" && body instanceof FormData) return body;
    return body.length ? body.slice() : undefined;
  }

  async function captureRawProxyResponse(response, runtime) {
    if (!response || typeof response.arrayBuffer !== "function") {
      throw new Error("企业微信 CLI 官方网关返回了不支持的响应对象");
    }

    // 二进制响应：流式上传 OSS，只把 envelope 回给 CLI（不进内存、不受 64MB 常量约束）。
    if (isWecomBinaryResponse(response)) {
      return await captureWecomBinaryResponse(response, runtime);
    }

    let buffer;
    try {
      buffer = await response.arrayBuffer();
    } catch (_) {
      throw new Error("企业微信 CLI 官方网关响应读取失败");
    }
    const body = new Uint8Array(buffer);
    if (body.byteLength > WECOM_RAW_PROXY_MAX_RESPONSE_BYTES) {
      throw new Error(
        `企业微信 CLI 响应超过 ${WECOM_RAW_PROXY_MAX_RESPONSE_BYTES} 字节上限`
      );
    }
    return {
      status: normalizeProxyStatus(response.status),
      headers: collectProxyResponseHeaders(response.headers),
      body
    };
  }

  async function captureWecomBinaryResponse(response, runtime) {
    const status = normalizeProxyStatus(response.status);

    // 非 2xx 的二进制不进入 envelope：原样回传状态码与字节，让 CLI 走错误分支。
    if (status < 200 || status > 299) {
      let buffer;
      try {
        buffer = await response.arrayBuffer();
      } catch (_) {
        throw new Error("企业微信 CLI 官方网关响应读取失败");
      }
      const body = new Uint8Array(buffer);
      if (body.byteLength > WECOM_RAW_PROXY_MAX_RESPONSE_BYTES) {
        throw new Error(
          `企业微信 CLI 响应超过 ${WECOM_RAW_PROXY_MAX_RESPONSE_BYTES} 字节上限`
        );
      }
      return {
        status,
        headers: collectProxyResponseHeaders(response.headers),
        body
      };
    }

    const envelope = await buildWecomBinaryEnvelope(response, runtime);
    return {
      status,
      // 换成 JSON，让 CLI 侧能识别出「这是文件 envelope 而不是原始响应」。
      headers: { "content-type": "application/json" },
      body: encodeProxyUtf8Bytes(JSON.stringify(envelope), runtime)
    };
  }

  function normalizeProxyStatus(value) {
    const status = Number(value);
    if (!Number.isInteger(status) || status < 100 || status > 599) {
      throw new Error("企业微信 CLI 官方网关响应状态码不合法");
    }
    return status;
  }

  function collectProxyResponseHeaders(source) {
    const result = {};
    const add = (rawName, rawValue) => {
      const name = String(rawName == null ? "" : rawName).trim().toLowerCase();
      if (!isValidHeaderName(name) || WECOM_RAW_PROXY_BLOCKED_RESPONSE_HEADERS.has(name)) return;
      const value = String(rawValue == null ? "" : rawValue);
      if (/\r|\n/.test(value) || value.length > 8192) return;
      result[name] = Object.prototype.hasOwnProperty.call(result, name)
        ? `${result[name]}, ${value}`
        : value;
    };
    if (!source) return result;
    if (typeof source.forEach === "function") {
      source.forEach((value, name) => add(name, value));
      return result;
    }
    if (typeof source.entries === "function") {
      for (const [name, value] of source.entries()) add(name, value);
      return result;
    }
    for (const name of Object.keys(source)) add(name, source[name]);
    return result;
  }

  function inspectWecomErrorCode(bodyBytes) {
    if (!bodyBytes || bodyBytes.length === 0 ||
        bodyBytes.length > WECOM_RAW_PROXY_MAX_INSPECT_BYTES) {
      return 0;
    }
    let outer;
    try {
      outer = JSON.parse(decodeProxyUtf8(bodyBytes));
    } catch (_) {
      return 0;
    }
    const outerCode = normalizeProxyErrorCode(outer && outer.errcode);
    if (outerCode !== 0) return outerCode;
    const directCode = normalizeProxyErrorCode(outer && outer.error && outer.error.code);
    if (directCode !== 0) return directCode;
    if (!outer || typeof outer.results_json !== "string") return 0;
    try {
      const inner = JSON.parse(outer.results_json);
      return normalizeProxyErrorCode(inner && inner.error && inner.error.code);
    } catch (_) {
      return 0;
    }
  }

  async function refreshProxyBearerToken(context, runtime) {
    const credentials = readProxyBotCredentials(context);
    const epochSeconds = Math.floor(resolveProxyNow(runtime) / 1000);
    const nonce = resolveProxyNonce(runtime);
    const signature = await proxySha256Hex(
      `${credentials.secret}${credentials.botId}${epochSeconds}${nonce}`,
      runtime
    );
    const fetchImpl = resolveProxyFetch(runtime);
    let response;
    try {
      response = await fetchImpl(WECOM_RAW_PROXY_AUTH_ENDPOINT, {
        method: "POST",
        headers: { accept: "application/json", "content-type": "application/json" },
        body: JSON.stringify({
          bot_id: credentials.botId,
          time: epochSeconds,
          nonce,
          signature,
          bind_source: 1
        }),
        redirect: "error"
      });
    } catch (_) {
      throw new Error("企业微信 Bot 鉴权请求失败");
    }
    const authBody = await readProxyJsonResponse(response, "企业微信 Bot 鉴权");
    const code = normalizeProxyErrorCode(authBody && authBody.errcode);
    if (!response.ok || code !== 0) {
      throw proxyApiError(
        code || response.status,
        safeProxyMessage(authBody && authBody.errmsg, "企业微信 Bot 鉴权失败", [
          credentials.botId,
          credentials.secret,
          signature
        ])
      );
    }
    const token = authBody && typeof authBody.token === "string" ? authBody.token.trim() : "";
    if (!token) throw proxyApiError(0, "企业微信 Bot 鉴权响应缺少 token");
    if (token.length > 4096 || /[\u0000-\u001f\u007f]/.test(token)) {
      throw proxyApiError(0, "企业微信 Bot 鉴权响应 token 不合法");
    }
    return token;
  }

  async function readProxyJsonResponse(response, name) {
    let text;
    try {
      text = await response.text();
    } catch (_) {
      throw proxyApiError(response && response.status, `${name}响应读取失败`);
    }
    if (text.length > WECOM_RAW_PROXY_MAX_INSPECT_BYTES) {
      throw proxyApiError(response && response.status, `${name}响应过大`);
    }
    try {
      return text ? JSON.parse(text) : null;
    } catch (_) {
      throw proxyApiError(response && response.status, `${name}返回非 JSON`);
    }
  }

  function serializeRawProxyResponse(response, runtime) {
    return {
      status: response.status,
      headers: response.headers,
      bodyBase64: encodeProxyBase64(response.body, runtime)
    };
  }

  // ── Base64 / UTF-8 / 运行时解析（原版保留） ────────────────

  function decodeProxyBase64(value, maxBytes, name, runtime) {
    const text = String(value == null ? "" : value);
    if (text.length > Math.ceil(maxBytes / 3) * 4 + 4) {
      throw new Error(`${name} 解码后不能超过 ${maxBytes} 字节`);
    }
    if (!/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(text)) {
      throw new Error(`${name} 必须是标准 Base64`);
    }
    const atobImpl = (runtime && runtime.atob) || globalThis.atob;
    if (typeof atobImpl !== "function") throw new Error("当前 Action 运行时不支持 Base64 解码");
    let binary;
    try {
      binary = atobImpl(text);
    } catch (_) {
      throw new Error(`${name} 必须是标准 Base64`);
    }
    if (binary.length > maxBytes) throw new Error(`${name} 解码后不能超过 ${maxBytes} 字节`);
    const bytes = new Uint8Array(binary.length);
    for (let index = 0; index < binary.length; index += 1) {
      bytes[index] = binary.charCodeAt(index);
    }
    return bytes;
  }

  function encodeProxyBase64(bytes, runtime) {
    const btoaImpl = (runtime && runtime.btoa) || globalThis.btoa;
    if (typeof btoaImpl !== "function") throw new Error("当前 Action 运行时不支持 Base64 编码");
    const chunks = [];
    const chunkSize = 0x8000;
    for (let offset = 0; offset < bytes.length; offset += chunkSize) {
      chunks.push(String.fromCharCode(...bytes.subarray(offset, offset + chunkSize)));
    }
    return btoaImpl(chunks.join(""));
  }

  function encodeProxyUtf8Bytes(text, runtime) {
    const encoder = (runtime && runtime.TextEncoder) ||
      (typeof TextEncoder !== "undefined" ? TextEncoder : undefined);
    if (typeof encoder !== "function") {
      throw new Error("当前 Action 运行时不支持 UTF-8 编码");
    }
    return new encoder().encode(text);
  }

  function decodeProxyUtf8(bytes) {
    if (typeof TextDecoder !== "function") {
      throw new Error("当前 Action 运行时不支持 UTF-8 解码");
    }
    return new TextDecoder("utf-8").decode(bytes);
  }

  async function proxySha256Hex(value, runtime) {
    const cryptoImpl = (runtime && runtime.crypto) || globalThis.crypto;
    if (!cryptoImpl || !cryptoImpl.subtle) {
      throw new Error("当前 Action 运行时不支持 Web Crypto SHA-256");
    }
    if (typeof TextEncoder !== "function") {
      throw new Error("当前 Action 运行时不支持 UTF-8 编码");
    }
    const digest = new Uint8Array(
      await cryptoImpl.subtle.digest("SHA-256", new TextEncoder().encode(value))
    );
    return Array.from(digest, byte => byte.toString(16).padStart(2, "0")).join("");
  }

  function resolveProxyFetch(runtime) {
    const fetchImpl = (runtime && runtime.fetch) || globalThis.fetch;
    if (typeof fetchImpl !== "function") throw new Error("当前 Action 运行时不支持 fetch");
    return fetchImpl;
  }

  function resolveProxyNow(runtime) {
    const value = runtime && typeof runtime.now === "function" ? runtime.now() : Date.now();
    if (!Number.isFinite(value) || value < 0) throw new Error("无法生成鉴权时间戳");
    return value;
  }

  function resolveProxyNonce(runtime) {
    if (runtime && typeof runtime.nonce === "function") {
      return requiredProxyString(runtime.nonce(), "nonce");
    }
    const cryptoImpl = (runtime && runtime.crypto) || globalThis.crypto;
    if (!cryptoImpl || typeof cryptoImpl.getRandomValues !== "function") {
      throw new Error("当前 Action 运行时不支持安全随机数");
    }
    const bytes = new Uint8Array(4);
    cryptoImpl.getRandomValues(bytes);
    const suffix = Array.from(bytes, byte => byte.toString(16).padStart(2, "0")).join("");
    return `cli_${Date.now()}_${suffix}`;
  }

  function normalizeProxyErrorCode(value) {
    if (value == null || value === "") return 0;
    const code = Number(value);
    return Number.isFinite(code) ? code : 0;
  }

  function safeProxyMessage(value, fallback, sensitiveValues) {
    const message = typeof value === "string" ? value.trim() : "";
    let sanitized = message ? message.slice(0, 300) : fallback;
    for (const sensitive of sensitiveValues || []) {
      const text = String(sensitive == null ? "" : sensitive);
      if (text) sanitized = sanitized.split(text).join("[REDACTED]");
    }
    return sanitized;
  }

  function proxyApiError(code, message) {
    const error = new Error(message);
    error.code = normalizeProxyErrorCode(code);
    return error;
  }

  function requiredProxyString(value, name) {
    const text = String(value == null ? "" : value).trim();
    if (!text) throw new Error(`当前影刀授权缺少 ${name}`);
    return text;
  }

  const params = (context && context.params) || {};
  return executeWecomRawProxy(params, context);
}
