// Trove Collector: an image, video or link becomes an asset in the local Trove
// library — in the collection you point at, from the context menu or by
// dragging the media onto the menu that opens under the cursor.
//
// The save path is built for reliability:
//
//   1. ping the service first, so "Trove is not running" costs one request
//      and a readable message instead of a full download that cannot land;
//   2. download in the browser and POST the bytes to /add — the request keeps
//      the page's session cookies and rides the <all_urls> host permission
//      past CORS, which is what gets through hotlink protection;
//   3. whatever the browser cannot get (HTTP error, network failure, an HTML
//      interstitial in place of the image, a body too big for worker memory)
//      falls through to POST /fetch, where Trove downloads the URL itself,
//      streaming to disk and carrying a browser-like User-Agent plus a
//      Referer of our choosing (see hotlink-sites.js).
//
// data: images are decoded locally (the name comes from the mime type);
// blob: images are read from the page via executeScript, because the blob
// registry is per-origin and this worker cannot fetch them.
//
// A named destination travels with the file rather than being applied here:
// the collect service writes files and never the database, so the target
// collection rides in the sidecar and the import job files the asset when it
// takes the file in.

// Chromium gets one service worker file and loads the shared modules itself;
// Firefox reads `background.scripts` from the manifest and has no
// `importScripts` in an event page.
if (typeof importScripts === 'function') {
  importScripts('preferences.js', 'collections.js', 'hotlink-sites.js');
}

// Above this size the bytes are not relayed through the service worker: a
// giant arrayBuffer is asking to be killed mid-save. /fetch streams to disk.
const RELAY_MAX = 64 * 1024 * 1024;

// Largest blob: payload pulled out of a page. executeScript results travel
// through JSON messaging, and the base64 detour costs a third more again.
const BLOB_MAX = 32 * 1024 * 1024;

// Image fetches may be big and slow; the ping must fail fast instead.
const FETCH_TIMEOUT = 120_000;
const PING_TIMEOUT = 3_000;

// The menu is drawn from the collection tree, and a browser has a real ceiling
// on registered items (Firefox's is the lower one). Deep or huge libraries are
// cut at this depth and this count rather than failing registration entirely.
const MENU_MAX_DEPTH = 3;
const MENU_MAX_ITEMS = 100;

// Which contexts get their own menu tree. `link` saves the file a link points
// at, so it wants a destination just as much as an image does.
const MENU_CONTEXTS = [
  { key: 'image', title: '保存到 Trove', contexts: ['image'] },
  { key: 'video', title: '保存到 Trove', contexts: ['video'] },
  { key: 'link', title: '保存链接文件到 Trove', contexts: ['link'] },
];

const ICON_ON = {
  16: 'icons/icon16.png',
  32: 'icons/icon32.png',
  128: 'icons/icon128.png',
};
const ICON_OFF = {
  16: 'icons/icon-gray16.png',
  32: 'icons/icon-gray32.png',
  128: 'icons/icon-gray128.png',
};

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
  'video/mp4': '.mp4',
  'video/webm': '.webm',
};

// The last tree we managed to read, so a menu can be built (and a drag menu
// answered) from what is known even while Trove is briefly unreachable.
let catalog = null;
let connected = false;

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

async function notify(message, title = 'Trove 采集器') {
  if (!(await TrovePrefs.get('notifications'))) return;
  try {
    chrome.notifications?.create({
      type: 'basic',
      iconUrl: 'icon.png',
      title,
      message,
    });
  } catch {
    // Notifications may be unavailable (no libnotify, restricted env);
    // never let a failed toast mask a successful save.
  }
}

// -- connection state -------------------------------------------------------

// The toolbar icon is the only standing indication of whether a save can work,
// so it is refreshed on a timer rather than only when a menu opens. Half a
// minute is Serpent's cadence and costs one /ping.
const PING_ALARM = 'trove-ping';

async function setConnected(now) {
  if (connected === now) return;
  connected = now;
  try {
    await chrome.action.setIcon({ path: now ? ICON_ON : ICON_OFF });
    await chrome.action.setBadgeText({ text: now ? '' : '未连' });
    await chrome.action.setBadgeBackgroundColor({ color: '#6b7280' });
    await chrome.action.setTitle({
      title: now ? 'Trove 采集器 · 已连接' : 'Trove 采集器 · 未连接',
    });
  } catch {
    // No toolbar action available (Firefox without the permission, a
    // relocated icon): the menu and the popup still say what is what.
  }
}

async function refreshConnection() {
  const port = await TrovePrefs.port();
  const up = await probeService(port);
  await setConnected(up.ok);
  if (up.ok) await refreshCatalog(port);
  await rebuildMenus();
  return up;
}

// One /ping before anything else: when Trove is down the user hears about it in
// one request, and no time goes into a file that cannot be saved anyway.
async function probeService(port) {
  try {
    const response = await fetch(`http://127.0.0.1:${port}/ping`, {
      signal: AbortSignal.timeout(PING_TIMEOUT),
    });
    const text = (await response.text().catch(() => '')).trim();
    if (response.ok && text.includes('trove')) return { ok: true };
    return { ok: false, error: `端口 ${port} 上不是 Trove 采集服务（${text ? text.slice(0, 40) : `HTTP ${response.status}`}）` };
  } catch {
    return { ok: false, error: `无法连接 Trove（127.0.0.1:${port}）—— 请先启动 Trove 桌面应用` };
  }
}

async function assertServiceUp(port) {
  const probe = await probeService(port);
  if (!probe.ok) {
    await setConnected(false);
    throw new Error(probe.error);
  }
  await setConnected(true);
}

async function refreshCatalog(port) {
  const loaded = await TroveCollections.load(port);
  if (loaded) catalog = loaded;
  return loaded;
}

// -- the target menu --------------------------------------------------------

// A menu item id carries its destination: `trove-save:<key>:<collectionId>`,
// with the empty id meaning "wherever saves normally go". The context key is in
// the id because each of the three menus needs its own copy of the tree —
// Chrome gives a parent's children no way to say which parent they came from.
function menuId(key, collection) {
  return `trove-save:${key}:${collection || ''}`;
}

function parseMenuId(id) {
  const parts = String(id).split(':');
  if (parts[0] !== 'trove-save') return null;
  return { key: parts[1] || '', collection: parts[2] || null };
}

let menuBuild = Promise.resolve();

// Rebuild serially: two overlapping `removeAll` + create rounds would leave a
// half-tree on the screen, which is worse than a menu a tick late.
function rebuildMenus() {
  menuBuild = menuBuild.then(buildMenus).catch(() => {});
  return menuBuild;
}

async function buildMenus() {
  await chrome.contextMenus.removeAll();
  const roots = catalog ? TroveCollections.tree(catalog.collections) : [];
  const recents = catalog
    ? await TroveCollections.recentIds(catalog.collections.map((c) => c.id))
    : [];
  const byId = new Map((catalog?.collections || []).map((c) => [c.id, c]));

  let budget = MENU_MAX_ITEMS;

  for (const context of MENU_CONTEXTS) {
    const parent = await chrome.contextMenus.create({
      id: `trove-root:${context.key}`,
      title: connected ? context.title : `${context.title}（未连接）`,
      contexts: context.contexts,
      enabled: connected,
    });
    if (!connected) continue;

    await chrome.contextMenus.create({
      id: menuId(context.key, null),
      parentId: parent,
      title: '默认位置（不指定合集）',
    });

    for (const id of recents.slice(0, TroveCollections.RECENT_PICK)) {
      const collection = byId.get(id);
      if (!collection) continue;
      await chrome.contextMenus.create({
        id: menuId(context.key, id),
        parentId: parent,
        title: `最近：${collection.path}`,
      });
    }

    await chrome.contextMenus.create({ id: `trove-sep:${context.key}`, parentId: parent, type: 'separator' });

    if (!roots.length) {
      await chrome.contextMenus.create({
        id: `trove-none:${context.key}`,
        parentId: parent,
        title: catalog ? '这个库还没有合集' : '读取合集列表失败',
        enabled: false,
      });
      continue;
    }

    for (const node of roots) {
      if (budget <= 0) break;
      budget -= await addCollectionMenu(context.key, node, parent, 1, budget);
    }
  }
}

// One collection as a submenu: the folder itself is a clickable child, because
// a menu item that has children cannot be chosen directly. A leaf gets no
// container at all — a submenu holding exactly one thing is one click deeper
// for nothing.
async function addCollectionMenu(key, node, parentId, depth, budget) {
  if (budget <= 0) return 0;
  if (!node.children.length) {
    await chrome.contextMenus.create({
      id: menuId(key, node.id),
      parentId,
      title: node.name,
    });
    return 1;
  }
  const container = await chrome.contextMenus.create({
    id: `trove-dir:${key}:${node.id}`,
    parentId,
    title: node.name,
  });
  let spent = 1;
  await chrome.contextMenus.create({
    id: menuId(key, node.id),
    parentId: container,
    title: `保存到「${node.path}」`,
  });
  spent += 1;
  if (depth >= MENU_MAX_DEPTH) return spent;
  for (const child of node.children) {
    if (budget - spent <= 0) break;
    spent += await addCollectionMenu(key, child, container, depth + 1, budget - spent);
  }
  return spent;
}

// -- saving -----------------------------------------------------------------

// Everything a save needs, from either entry point. `collection` is a
// collection id or null for the library's default place; `isLink` means the
// menu promised a file, so an HTML answer must not be landed.
async function save({ url, source, collection, isLink = false }) {
  const port = await TrovePrefs.port();
  await assertServiceUp(port);
  let savedName;
  if (url.startsWith('data:')) {
    // data: URLs decode locally, no network involved.
    const response = await fetch(url);
    const mime = response.headers.get('content-type') || '';
    const bytes = new Uint8Array(await response.arrayBuffer());
    savedName = await postAdd(port, bytes, generatedName(mime), source, collection);
  } else if (url.startsWith('blob:')) {
    const { type, bytes } = await pageBlobBytes(tabIdOfSource(), url);
    savedName = await postAdd(port, bytes, generatedName(type), source, collection);
  } else {
    savedName = await saveHttp(port, url, source, isLink, collection);
  }
  if (collection) await TroveCollections.remember(collection);
  return savedName;
}

// The blob path needs a tab to run in, and only the context-menu path knows
// which one; a drag menu does not offer blob: sources at all.
let sourceTabId = null;
function tabIdOfSource() {
  return sourceTabId;
}

// POST the raw bytes to /add; returns the name the file actually landed
// under (the service prefixes it) for the confirmation toast.
async function postAdd(port, bytes, name, source, collection) {
  let query = `?filename=${encodeURIComponent(name)}&source=${encodeURIComponent(source)}`;
  if (collection) query += `&collection=${encodeURIComponent(collection)}`;
  if (await TrovePrefs.get('focusAfterSave')) query += '&focus=1';
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
async function saveHttp(port, url, source, isLink, collection) {
  const relay = await relayDownload(url);
  if (relay.ok) {
    const name = fileNameFromUrl(url, relay.contentType);
    return postAdd(port, relay.bytes, name, source, collection);
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
    return await serverFetch(port, url, name, source, collection);
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
// the Referer the hotlink table asks for, falling back to the capturing page.
// Returns the landed file name.
async function serverFetch(port, url, name, source, collection) {
  const body = {
    url,
    name,
    source,
    referer: TroveHotlink.refererFor(url, source),
    reject_html: true,
  };
  if (collection) body.collection = collection;
  if (await TrovePrefs.get('focusAfterSave')) body.focus = true;
  const saved = await fetch(`http://127.0.0.1:${port}/fetch`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
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

// -- wiring -----------------------------------------------------------------

chrome.runtime.onInstalled.addListener(() => {
  TroveHotlink.installRules().catch(() => {});
  chrome.alarms.create(PING_ALARM, { periodInMinutes: 0.5 });
  refreshConnection().catch(() => {});
});

chrome.runtime.onStartup.addListener(() => {
  TroveHotlink.installRules().catch(() => {});
  chrome.alarms.create(PING_ALARM, { periodInMinutes: 0.5 });
  refreshConnection().catch(() => {});
});

chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === PING_ALARM) refreshConnection().catch(() => {});
});

chrome.storage.onChanged.addListener((changes, area) => {
  TrovePrefs.invalidate();
  if (area === 'local' && changes.port) refreshConnection().catch(() => {});
});

// Chromium-only, and the reason the tree is worth re-reading: the right-click
// is about to open, so a collection created since the last ping still shows up.
// `refresh` is what re-reads the menu after this handler has rebuilt it.
if (chrome.contextMenus.onShown) {
  chrome.contextMenus.onShown.addListener(async (info, tab) => {
    sourceTabId = tab?.id ?? sourceTabId;
    const port = await TrovePrefs.port();
    const probe = await probeService(port);
    await setConnected(probe.ok);
    if (probe.ok) {
      await refreshCatalog(port);
      await rebuildMenus();
      chrome.contextMenus.refresh?.();
    }
  });
}

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  const target = parseMenuId(info.menuItemId);
  if (!target) return;
  sourceTabId = tab?.id ?? null;
  const url = info.srcUrl || info.linkUrl;
  if (!url) {
    await notify('这一项没有可保存的地址');
    return;
  }
  const source = tab?.url || info.pageUrl || url;
  try {
    const savedName = await save({
      url,
      source,
      collection: target.collection,
      isLink: target.key === 'link',
    });
    const where = target.collection
      ? `到「${catalog?.collections.find((c) => c.id === target.collection)?.path || '合集'}」`
      : '';
    await notify(`已保存${where}：${savedName}`);
  } catch (error) {
    await notify(error.refusal ? error.message : `保存失败：${error.message}`);
  }
});

// The drag menu lives in the page and knows nothing about the service; every
// request it makes goes through here.
chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (message?.type === 'trove-drag-save') {
    // A blob: source can only be read inside the page that made it, and the
    // drag does not say which tab that was — the message does.
    sourceTabId = sender.tab?.id ?? sourceTabId;
    save({
      url: message.url,
      source: message.source || sender.tab?.url || message.url,
      collection: message.collection || null,
    })
      .then((file) => sendResponse({ ok: true, file }))
      .catch((error) => sendResponse({ ok: false, error: error.message }));
    return true; // answered asynchronously
  }
  if (message?.type === 'trove-status') {
    // The menu needs a tree before the drag even starts, and the popup wants to
    // say which library it is on; the answer comes from the last catalog read,
    // so a slow Trove delays nothing the user can see.
    sendResponse({
      connected,
      library: catalog?.library || null,
      collections: catalog?.collections || null,
    });
    return false;
  }
  return false;
});
