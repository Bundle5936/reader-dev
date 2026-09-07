/**
 * 图片代理开关 reader_image_proxy（默认关）。
 * 开启后远端 http(s) 图片经后端 /assets/proxy 回源（复用书源登录态/UA/Referer 与
 * WebP 转换，防盗链与私网拦截由服务端统一处理）。
 *
 * HEIC/HEIF 图片即使没有打开开关也强制走代理：Chrome/WebView 通常不能直接解码，
 * 由服务端统一转成 JPEG/WebP。代理 URL 显式携带当前会话 token，因为 img 请求不会
 * 自动带上 axios 的 query 参数或 Authorization header。
 */

const PROXY_KEY = 'reader_image_proxy'
const ACCESS_TOKEN_KEY = 'reader_access_token'

export function imageProxyEnabled(): boolean {
  try {
    return localStorage.getItem(PROXY_KEY) === '1'
  } catch {
    return false
  }
}

export function setImageProxyEnabled(on: boolean): void {
  try {
    localStorage.setItem(PROXY_KEY, on ? '1' : '0')
  } catch {
    /* ignore */
  }
}

function currentAccessToken(): string {
  try {
    return localStorage.getItem(ACCESS_TOKEN_KEY) || sessionStorage.getItem(ACCESS_TOKEN_KEY) || ''
  } catch {
    return ''
  }
}

function isHeifUrl(url: string): boolean {
  try {
    return /\.(?:heic|heif)$/i.test(new URL(url).pathname)
  } catch {
    return /\.(?:heic|heif)(?:[?#]|$)/i.test(url)
  }
}

export interface ProxyImageOptions {
  /** 不论 reader_image_proxy 开关，均经后端代理；适合书源封面等第三方媒体。 */
  force?: boolean
}

export function proxyImageUrl(
  url: string | null | undefined,
  options: ProxyImageOptions = {},
): string | null | undefined {
  if (!url || !/^https?:\/\//i.test(url)) return url
  if (!options.force && !imageProxyEnabled() && !isHeifUrl(url)) return url

  const params = new URLSearchParams({ url })
  const accessToken = currentAccessToken()
  if (accessToken) params.set('accessToken', accessToken)
  return `/assets/proxy?${params.toString()}`
}
