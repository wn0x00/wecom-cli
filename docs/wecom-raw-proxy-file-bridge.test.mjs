/**
 * wecom-raw-proxy-file-bridge.js 的行为验证
 *
 *   node wecom-raw-proxy-file-bridge.test.mjs
 *
 * 覆盖：JSON 透传回归 / 上传桥 / 下载桥 / 描述符 URL 直用（与飞书对齐）/
 *       路径与描述符约束 / 853004 刷新回归 / header 黑名单回归
 */
import fs from 'node:fs'
import { Readable } from 'node:stream'

const src = fs.readFileSync(new URL('./wecom-raw-proxy-file-bridge.js', import.meta.url), 'utf8')

// 断言 host 校验确实已移除（与飞书对齐的决策记录）
for (const removed of [
  'function validateWecomBridgeFileUrl',
  'function isWecomBridgeBlockedHost',
  'const WECOM_RAW_PROXY_FILE_BRIDGE_ALLOWED_HOST_SUFFIXES',
]) {
  if (src.includes(removed)) {
    throw new Error(`期望已移除的 host 校验仍存在于脚本中: ${removed}`)
  }
}

globalThis.Readable = Readable
globalThis.iPaaSFileUpload = async (stream, key, name) => {
  let n = 0
  for await (const c of stream) n += c.length
  return `https://files.example.com/read/${key}|bytes=${n}`
}

// 模拟 iPaaS 运行时的全局 FormData（npm form-data 包，非 undici 原生）：
// append(field, Buffer, {filename, contentType, knownLength}) 可用；
// append(field, File/Blob) 会按流处理并抛 "source.on is not a function"
// —— 这是线上 502 的真实报错，保留为回归守卫。
// getBuffer()/getHeaders() 按 form-data 包语义最小实现（序列化成完整 multipart）。
const MOCK_BOUNDARY = 'mockBoundary123'
class FormDataMock {
  constructor() { this._fields = [] }
  append(field, value, options) {
    if (typeof value === 'string') { this._fields.push({ field, text: value }); return }
    if (Buffer.isBuffer(value)) { this._fields.push({ field, buffer: value, options }); return }
    throw new Error('source.on is not a function')
  }
  get(field) {
    const f = this._fields.find(x => x.field === field)
    return f && ('text' in f ? f.text : f.buffer)
  }
  getHeaders() {
    return { 'content-type': `multipart/form-data; boundary=${MOCK_BOUNDARY}` }
  }
  getBuffer() {
    const chunks = []
    for (const f of this._fields) {
      chunks.push(Buffer.from(`--${MOCK_BOUNDARY}\r\n`))
      if ('text' in f) {
        chunks.push(Buffer.from(`Content-Disposition: form-data; name="${f.field}"\r\n\r\n${f.text}\r\n`))
      } else {
        chunks.push(Buffer.from(
          `Content-Disposition: form-data; name="${f.field}"; filename="${f.options.filename}"\r\n` +
          `Content-Type: ${f.options.contentType}\r\n\r\n`,
        ))
        chunks.push(f.buffer)
        chunks.push(Buffer.from('\r\n'))
      }
    }
    chunks.push(Buffer.from(`--${MOCK_BOUNDARY}--\r\n`))
    return Buffer.concat(chunks)
  }
}
globalThis.FormData = FormDataMock

const b64 = (s) => Buffer.from(s, 'utf8').toString('base64')
const unb64 = (s) => Buffer.from(s, 'base64').toString('utf8')
const main = new Function(src + '\nreturn main;')()

const json = (obj, status = 200) =>
  new Response(JSON.stringify(obj), { status, headers: { 'content-type': 'application/json' } })

function call(params, extraAuth) {
  const { token = 'tok', inputs } = extraAuth || {}
  return main({ context: { params, property: { auth: { outputs: { token }, inputs: inputs || {} } } } })
}

const OSS_HOST = 'files.example.com'
const isOssRead = (url) => String(url).startsWith(`https://${OSS_HOST}/read/`)
const ossReadResponse = (bytes, headers) =>
  new Response(new Uint8Array(bytes), { status: 200, headers: headers || { 'content-type': 'application/pdf' } })

let pass = 0
let fail = 0
const ok = (name, cond, extra) => {
  if (cond) {
    pass++
    console.log('  ✅', name)
  } else {
    fail++
    console.log('  ❌', name, extra ?? '')
  }
}

// ── 1. 回归：纯 JSON 透传（改造前行为必须一字不变） ──
console.log('1) JSON 透传回归')
{
  const upstream = { errcode: 0, errmsg: 'ok', results_json: JSON.stringify({ result: '{"ok":true}' }) }
  globalThis.fetch = async () => json(upstream)
  const out = await call({ provider: 'wecom', method: 'POST', path: '/identity/whoami', query: '', headers: {}, bodyBase64: b64('{}') })
  ok('status=200', out.status === 200, out.status)
  ok('bodyBase64 原样回传', JSON.parse(unb64(out.bodyBase64)).errcode === 0, unb64(out.bodyBase64))
}

// ── 2. 上传桥 ──
console.log('2) 上传桥')
{
  let seenUrl = ''
  let seenCT = ''
  let seenBody = null
  globalThis.fetch = async (url, init = {}) => {
    if (isOssRead(url)) return ossReadResponse([1, 2, 3, 4, 5])
    seenUrl = String(url)
    seenBody = init.body
    seenCT = new Headers(init.headers).get('content-type') || ''
    return json({ errcode: 0, errmsg: 'ok', media_id: 'M1' })
  }
  const descriptor = {
    type: 'file',
    media: { url: `https://${OSS_HOST}/read/a.pdf`, filename: 'a.pdf', content_type: 'application/pdf' },
  }
  const out = await call({
    provider: 'wecom',
    method: 'POST',
    path: '/file/upload',
    query: '',
    headers: { 'content-type': 'multipart/form-data; boundary=STALE' },
    bodyBase64: b64(JSON.stringify(descriptor)),
  })
  ok('打到企微固定域名', seenUrl === 'https://qyapi.weixin.qq.com/cli/file/upload', seenUrl)
  // 运行时 fetch（undici 形态）不认 form-data 实例：直接传会把 body 弱转成
  // "[object FormData]" 文本 → 企微 44001 媒体数据为空。脚本必须发已序列化的字节。
  ok('body 是已序列化 Buffer（非 form-data 实例）', Buffer.isBuffer(seenBody), String(seenBody && seenBody.constructor && seenBody.constructor.name))
  const bodyText = Buffer.isBuffer(seenBody) ? seenBody.toString('latin1') : ''
  ok('multipart 含 boundary 与文件字节', bodyText.includes(`--${MOCK_BOUNDARY}`) && bodyText.includes('filename="a.pdf"') && bodyText.includes('Content-Type: application/pdf') && bodyText.includes(Buffer.from([1, 2, 3, 4, 5]).toString('latin1')), bodyText.slice(0, 200))
  ok('文本字段 type=file 保留', bodyText.includes('name="type"') && bodyText.includes('file'))
  ok('content-type 带当次 boundary', seenCT === `multipart/form-data; boundary=${MOCK_BOUNDARY}`, seenCT)
  ok('旧 boundary 未被透传', !seenCT.includes('STALE'), JSON.stringify(seenCT))
  ok('媒体 id 回传', JSON.parse(unb64(out.bodyBase64)).media_id === 'M1', unb64(out.bodyBase64))
}

// ── 3. 下载桥 ──
console.log('3) 下载桥')
{
  globalThis.fetch = async () =>
    new Response(new Uint8Array([9, 8, 7]), {
      status: 200,
      headers: { 'content-type': 'application/octet-stream', 'content-disposition': "attachment; filename*=UTF-8''re*port.pdf" },
    })
  const out = await call({ provider: 'wecom', method: 'POST', path: '/media/download', query: '', headers: {}, bodyBase64: b64('{}') })
  const env = JSON.parse(unb64(out.bodyBase64))
  ok('回的是 __ipaas_file envelope', !!env.__ipaas_file, unb64(out.bodyBase64))
  ok('headers 换成 application/json', out.headers['content-type'] === 'application/json', JSON.stringify(out.headers))
  ok('content_type 原样保留', env.__ipaas_file.content_type === 'application/octet-stream')
  ok('文件名已净化（* → _）', env.__ipaas_file.filename === 're_port.pdf', env.__ipaas_file.filename)
  ok('流式上传拿到 3 字节', String(env.__ipaas_file.oss_url).includes('bytes=3'), env.__ipaas_file.oss_url)
}

// ── 4. 描述符 URL 直用（与飞书对齐，不做 host 校验） ──
console.log('4) 描述符 URL 直用')
{
  const hosts = [
    `https://${OSS_HOST}/read/a.bin`,
    'https://another-bucket.cos.ap-shanghai.myqcloud.com/x.bin',
    'https://cdn.internal-mirror.example.com/x.bin',
  ]
  for (const url of hosts) {
    let hit = ''
    globalThis.fetch = async (u) => {
      if (String(u) === url) {
        hit = String(u)
        return new Response(new Uint8Array([1]), { status: 200, headers: { 'content-type': 'application/pdf' } })
      }
      return json({ errcode: 0 })
    }
    await call({
      provider: 'wecom',
      method: 'POST',
      path: '/file/upload',
      query: '',
      headers: { 'content-type': 'multipart/form-data' },
      bodyBase64: b64(JSON.stringify({ media: { url, filename: 'a.bin' } })),
    })
    ok(`直连 ${url}`, hit === url, hit)
  }

  // 约定的 1 个文件 + 体积上限仍然生效（功能性限制，非 SSRF 防护）
  let threw = ''
  globalThis.fetch = async () => json({ errcode: 0 })
  try {
    await call({
      provider: 'wecom',
      method: 'POST',
      path: '/file/upload',
      query: '',
      headers: { 'content-type': 'multipart/form-data' },
      bodyBase64: b64(JSON.stringify({ a: { url: `https://${OSS_HOST}/1`, filename: '1' }, b: { url: `https://${OSS_HOST}/2`, filename: '2' } })),
    })
  } catch (e) {
    threw = e.message
  }
  ok('多文件仍被拒', threw.includes('只允许'), threw)
}

// ── 5. 路径与描述符约束 ──
console.log('5) 路径与描述符约束')
{
  globalThis.fetch = async () => json({})
  let threw = ''
  try {
    await call({ provider: 'wecom', method: 'POST', path: '/identity/whoami', query: '', headers: { 'content-type': 'multipart/form-data' }, bodyBase64: b64('{}') })
  } catch (e) {
    threw = e.message
  }
  ok('非上传路径上的 multipart 被拒', threw.includes('白名单'), threw)

  let threw2 = ''
  try {
    await call({ provider: 'wecom', method: 'POST', path: '/file/upload', query: '', headers: { 'content-type': 'multipart/form-data' }, bodyBase64: b64(JSON.stringify({ type: 'file' })) })
  } catch (e) {
    threw2 = e.message
  }
  ok('无文件描述符被拒', threw2.includes('未包含文件描述符'), threw2)

  let threw3 = ''
  try {
    await call({ provider: 'wecom', method: 'POST', path: '/file/upload', query: '', headers: { 'content-type': 'multipart/form-data' }, bodyBase64: b64('not-json') })
  } catch (e) {
    threw3 = e.message
  }
  ok('非 JSON 描述符被拒', threw3.includes('JSON 描述符'), threw3)
}

// ── 6. 853004 刷新回归 ──
console.log('6) 853004 刷新回归')
{
  let callCount = 0
  globalThis.fetch = async (url) => {
    if (String(url).includes('get_cli_config')) return json({ errcode: 0, token: 'newtok' })
    callCount += 1
    return callCount === 1 ? json({ errcode: 853004, errmsg: 'expired' }) : json({ errcode: 0, errmsg: 'ok', results_json: '{}' })
  }
  const out = await call(
    { provider: 'wecom', method: 'POST', path: '/identity/whoami', query: '', headers: {}, bodyBase64: b64('{}') },
    { token: 'oldtok', inputs: { bot_id: 'b', bot_secret: 's' } },
  )
  ok('刷新后重放成功', JSON.parse(unb64(out.bodyBase64)).errcode === 0 && callCount === 2, `call=${callCount}`)
}

// ── 7. header 黑名单回归 ──
console.log('7) header 黑名单回归')
{
  let seen = null
  globalThis.fetch = async (url, init = {}) => {
    if (isOssRead(url)) return ossReadResponse([1])
    seen = new Headers(init.headers)
    return json({ errcode: 0 })
  }
  await call({
    provider: 'wecom',
    method: 'POST',
    path: '/file/upload',
    query: '',
    headers: {
      'content-type': 'multipart/form-data',
      cookie: 'C=1',
      'x-ipass-session-id': 's',
      'x-forwarded-for': '1.2.3.4',
      'xybot-product': 'x',
    },
    bodyBase64: b64(JSON.stringify({ media: { url: `https://${OSS_HOST}/read/a`, filename: 'a' } })),
  })
  ok('cookie 被剥离', !seen.get('cookie'))
  ok('x-ipass-* 被剥离', !seen.get('x-ipass-session-id'))
  ok('x-forwarded-* 被剥离', !seen.get('x-forwarded-for'))
  ok('xybot-product 被剥离', !seen.get('xybot-product'))
  ok('authorization 由平台注入', seen.get('authorization') === 'Bearer tok', seen.get('authorization'))
}

console.log(`\n通过 ${pass} / 失败 ${fail}`)
process.exit(fail ? 1 : 0)
