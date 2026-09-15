// Trove Collector: right-click an image -> save into the local Trove
// library via the collect service (POST /add). The service must be running
// (Trove open); the popup can change the port and ping the service.

const DEFAULT_PORT = 23916;

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
};

function ensureExt(name, contentType) {
  if (/\.[a-z0-9]{2,5}$/i.test(name)) return name;
  const type = (contentType || '').split(';')[0].trim().toLowerCase();
  return EXT_BY_TYPE[type] ? name + EXT_BY_TYPE[type] : name;
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
  try {
    // Port must be read before building any URL: storage.local is async,
    // and interpolating the promise itself yields "[object Promise]".
    const { port: stored } = await chrome.storage.local.get('port');
    const port = Number(stored) || DEFAULT_PORT;
    // fetch() from the background page; <all_urls> host permission makes
    // cross-origin image reads legal without CORS headers.
    const response = await fetch(url);
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const name = ensureExt(
      decodeURIComponent(url.split(/[?#]/)[0].split('/').pop() || 'collected.bin'),
      response.headers.get('content-type'),
    );
    const bytes = await response.arrayBuffer();
    const query = `?filename=${encodeURIComponent(name)}&source=${encodeURIComponent(source)}`;
    const saved = await fetch(`http://127.0.0.1:${port}/add${query}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/octet-stream' },
      body: bytes,
    });
    const result = await saved.json();
    if (result.ok) notify(`已保存到 Trove：${name}`);
    else notify(`Trove 拒绝：${result.error || '未知错误'}`);
  } catch (error) {
    notify(`保存失败：${error.message}`);
  }
});
