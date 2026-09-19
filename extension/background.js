// Trove Collector: right-click an image or link -> save into the local Trove
// library via the collect service. The save path is built for reliability:
//
//   1. ping the service first, so "Trove is not running" costs one request
//      and a readable message instead of a full download that cannot land;
//   2. download in the browser and POST the bytes to /add — the request keeps
//      the page's session cookies and rides the <all_urls> host permission
//      past CORS, which is what gets through hotlink protection;
//   3. whatever the browser cannot get (HTTP error, network failure, an HTML
//      interstitial in place of the image, a body too big for worker memory)
//      falls through to POST /fetch, where Trove downloads the URL itself,
//      streaming to disk and carrying a browser-like User-Agent plus the
//      page as Referer.
//
// data: images are decoded locally (the name comes from the mime type);
// blob: images are read from the page via executeScript, because the blob
// registry is per-origin and this worker cannot fetch them.

const DEFAULT_PORT = 23916;

// Above this size the bytes are not relayed through the service worker: a
// giant arrayBuffer is asking to be killed mid-save. /fetch streams to disk.
const RELAY_MAX = 64 * 1024 * 1024;

// Largest blob: payload pulled out of a page. executeScript results travel
// through JSON messaging, and the base64 detour costs a third more again.
const BLOB_MAX = 32 * 1024 * 1024;

// Image fetches may be big and slow; the ping must fail fast instead.
const FETCH_TIMEOUT = 120_000;
const PING_TIMEOUT = 3_000;

// Map response content types to file extensions for URLs that carry none
// (e.g. Bing thumbnails like .../OIP-C.4tnBCsiBG1gm9QN0GcsVpAHaG9).
const EXT_BY_TYPE = {
  'image/jpeg': '.jpg',
  'image/png': '.png',
  'image/gif': '.gif',
  'image/webp': '.webp',
  'image/avif': '.avif',
  'image/bmp': '.bmp',
  'image/svg+xml': '.svg',
  'image/x-icon': '.ico',
  'image/vnd.microsoft.icon': '.ico',
  'image/apng': '.apng',
  'image/tiff': '.tif',
  'image/heic': '.heic',
  'image/heif': '.heif',
};

function ensureExt(name, contentType) {
  if (/\.[a-z0-9]{2,5}$/i.test(name)) return name;
  const type = (contentType || '').split(';')[0].trim().toLowerCase();
  return EXT_BY_TYPE[type] ? name + EXT_BY_TYPE[type] : name;
}

// decodeURIComponent throws on malformed percent-encoding (a segment ending
// in a bare "%", say); the raw segment is still a usable file name.
function fileNameFromUrl(url, contentType) {
  const segment = url.split(/[?#]/)[0].split('/').pop() || '';
  let name;
  try {
    name = decodeURIComponent(segment);
  } catch {
    name = segment;
  }
  return ensureExt(name || 'collected.bin', contentType);
}

// Name for bytes that arrive without one (data:/blob:): the mime picks the
// suffix, the timestamp keeps repeated captures apart.
function generatedName(mime) {
  const d = new Date();
  const p = (n) => String(n).padStart(2, '0');
  const stamp = `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}-${p(d.getHours())}${p(d.getMinutes())}${p(d.getSeconds())}`;
  const type = (mime || '').split(';')[0].trim().toLowerCase();
  return `collected-${stamp}${EXT_BY_TYPE[type] || '.bin'}`;
}

function isHtml(contentType) {
  return (contentType || '').split(';')[0].trim().toLowerCase().startsWith('text/html');
}

function notify(message) {
  try {
    chrome.notifications?.create({
      type: 'basic',
      iconUrl: 'icon.png',
      title: 'Trove 采集器',
      message,
    });
  } catch {
    // Notifications may be unavailable (no libnotify, restricted env);
    // never let a failed toast mask a successful save.
  }
}

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.create({
    id: 'trove-save-image',
    title: '保存图片到 Trove',
    contexts: ['image'],
  });
  chrome.contextMenus.create({
    id: 'trove-save-link',
    title: '保存链接文件到 Trove',
    contexts: ['link'],
  });
});

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  const url = info.srcUrl || info.linkUrl;
  if (!url) return;
  const source = tab?.url || info.pageUrl || url;
  const isLink = info.menuItemId === 'trove-save-link';
  // Port must be read before building any URL: storage.local is async,
  // and interpolating the promise itself yields "[object Promise]".
  const { port: stored } = await chrome.storage.local.get('port');
  const port = Number(stored) || DEFAULT_PORT;
  try {
    await assertServiceUp(port);
    let savedName;
    if (url.startsWith('data:')) {
      // data: URLs decode locally, no network involved.
      const response = await fetch(url);
      const mime = response.headers.get('content-type') || '';
      const bytes = new Uint8Array(await response.arrayBuffer());
      savedName = await postAdd(port, bytes, generatedName(mime), source);
    } else if (url.startsWith('blob:')) {
      const { type, bytes } = await pageBlobBytes(tab?.id, url);
      savedName = await postAdd(port, bytes, generatedName(type), source);
    } else {
      savedName = await saveHttp(port, url, source, isLink);
    }
    notify(`已保存到 Trove：${savedName}`);
  } catch (error) {
    notify(error.refusal ? error.message : `保存失败：${error.message}`);
  }
});

// One /ping before anything else: when Trove is down the user hears about it
// in one request, and no time goes into a file that cannot be saved anyway.
async function assertServiceUp(port) {
  let response;
  try {
    response = await fetch(`http://127.0.0.1:${port}/ping`, {
      signal: AbortSignal.timeout(PING_TIMEOUT),
    });
  } catch {
    throw new Error(`无法连接 Trove（127.0.0.1:${port}）—— 请先启动 Trove 桌面应用`);
  }
  const text = (await response.text().catch(() => '')).trim();
  if (!response.ok || !text.includes('trove')) {
    const hint = text ? text.slice(0, 40) : `HTTP ${response.status}`;
    throw new Error(`端口 ${port} 上不是 Trove 采集服务（${hint}）`);
  }
}

// POST the raw bytes to /add; returns the name the file actually landed
// under (the service prefixes it) for the confirmation toast.
async function postAdd(port, bytes, name, source) {
  const query = `?filename=${encodeURIComponent(name)}&source=${encodeURIComponent(source)}`;
  const saved = await fetch(`http://127.0.0.1:${port}/add${query}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/octet-stream' },
    body: bytes,
    signal: AbortSignal.timeout(FETCH_TIMEOUT),
  });
  const result = await jsonOf(saved);
  if (!result) throw new Error(`端口 ${port} 的响应不是 Trove 服务（HTTP ${saved.status}）`);
  if (!saved.ok) throw new Error(`Trove 拒绝：${result.error || `HTTP ${saved.status}`}`);
  return result.file || name;
}

// The service answers JSON; a non-JSON body means something else lives on
// this port (or a proxy answered). Parse out of band so the failure names
// the status instead of "Unexpected token".
async function jsonOf(response) {
  const text = await response.text().catch(() => '');
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

// The main path for http(s) URLs: relay the bytes through the browser. The
// request keeps the page's session cookies and bypasses CORS via the
// <all_urls> host permission, so logged-in CDNs and hotlink protection
// mostly just work. Anything the browser cannot get falls back to /fetch.
async function saveHttp(port, url, source, isLink) {
  const relay = await relayDownload(url);
  if (relay.ok) {
    const name = fileNameFromUrl(url, relay.contentType);
    return postAdd(port, relay.bytes, name, source);
  }
  // The menu promises a *file*; a 200 HTML answer behind "save link" is the
  // page itself, not something the service can import.
  if (isLink && relay.status === 200 && isHtml(relay.contentType)) {
    const refusal = new Error('链接指向的是网页而非文件，未保存');
    refusal.refusal = true;
    throw refusal;
  }
  const name = fileNameFromUrl(url, relay.contentType);
  try {
    return await serverFetch(port, url, name, source);
  } catch (error) {
    const why = relay.status === 200
      ? '浏览器只取回一个网页'
      : relay.status === 0
        ? '浏览器无法连接该地址'
        : `浏览器抓取失败（HTTP ${relay.status}）`;
    throw new Error(`${why}，Trove 服务端下载也失败：${error.message}`);
  }
}

// Download in the browser. Never throws: every failure becomes { ok: false }
// and the caller tries the server-side path instead. An HTML body is a
// failure too — an image URL answering with a page smells like a hotlink
// interstitial, and neither belongs in the library.
async function relayDownload(url) {
  try {
    const response = await fetch(url, {
      credentials: 'include',
      signal: AbortSignal.timeout(FETCH_TIMEOUT),
    });
    const contentType = response.headers.get('content-type') || '';
    // A missing content-length reads as 0: only a *reported* size past the
    // cap diverts to /fetch, chunked bodies still relay.
    const length = Number(response.headers.get('content-length') || 0);
    if (response.ok && length <= RELAY_MAX && !isHtml(contentType)) {
      return { ok: true, contentType, bytes: new Uint8Array(await response.arrayBuffer()) };
    }
    return { ok: false, contentType, status: response.status };
  } catch {
    return { ok: false, contentType: '', status: 0 };
  }
}

// POST /fetch: Trove downloads the URL itself (streams straight to disk, so
// the size no longer costs worker memory) with a browser-like User-Agent and
// the capturing page as Referer. Returns the landed file name.
async function serverFetch(port, url, name, source) {
  const saved = await fetch(`http://127.0.0.1:${port}/fetch`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ url, name, source, referer: source, reject_html: true }),
    signal: AbortSignal.timeout(FETCH_TIMEOUT),
  });
  const result = await jsonOf(saved);
  if (!result) throw new Error(`HTTP ${saved.status}`);
  if (!saved.ok) throw new Error(result.error || `HTTP ${saved.status}`);
  return result.file || name;
}

// blob: URLs are registered per-origin, so this worker cannot fetch one.
// Run the fetch inside the page instead and relay the bytes back as base64.
async function pageBlobBytes(tabId, url) {
  if (!tabId) throw new Error('找不到来源标签页，无法读取页面内的 blob 图像');
  let injection;
  try {
    [injection] = await chrome.scripting.executeScript({
      target: { tabId },
      func: async (blobUrl, max) => {
        const response = await fetch(blobUrl);
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        const blob = await response.blob();
        if (blob.size > max) {
          throw new Error(`文件超过 ${Math.round(max / 1048576)}MB，无法从页面读取`);
        }
        const dataUrl = await new Promise((resolve, reject) => {
          const reader = new FileReader();
          reader.onload = () => resolve(String(reader.result));
          reader.onerror = () => reject(new Error('文件读取失败'));
          reader.readAsDataURL(blob);
        });
        return { type: blob.type, base64: dataUrl.slice(dataUrl.indexOf(',') + 1) };
      },
      args: [url, BLOB_MAX],
    });
  } catch (error) {
    throw new Error(`无法在此页面读取 blob 图像：${error.message}`);
  }
  const result = injection?.result;
  if (!result?.base64) throw new Error('页面内未能取得图像数据');
  return { type: result.type, bytes: base64Bytes(result.base64) };
}

function base64Bytes(base64) {
  const binary = atob(base64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
  return bytes;
}
