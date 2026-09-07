// Trove Collector: right-click an image -> save into the local Trove
// library via the collect service (POST /add). The service must be running
// (Trove open); the popup can change the port and ping the service.

const DEFAULT_PORT = 23916;

async function port() {
  const { port } = await chrome.storage.local.get('port');
  return Number(port) || DEFAULT_PORT;
}

function endpoint(path, query = '') {
  return `http://127.0.0.1:${port()}${path}${query}`;
}

function notify(message) {
  chrome.notifications.create({
    type: 'basic',
    iconUrl: 'icon.png',
    title: 'Trove Collector',
    message,
  });
}

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.create({
    id: 'trove-save-image',
    title: 'Save image to Trove',
    contexts: ['image'],
  });
  chrome.contextMenus.create({
    id: 'trove-save-link',
    title: 'Save linked file to Trove',
    contexts: ['link'],
  });
});

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  const url = info.srcUrl || info.linkUrl;
  if (!url) return;
  const source = tab?.url || info.pageUrl || url;
  const name = decodeURIComponent(url.split(/[?#]/)[0].split('/').pop() || 'collected.bin');
  try {
    // fetch() from the service worker; <all_urls> host permission makes
    // cross-origin image reads legal without CORS headers.
    const response = await fetch(url);
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const bytes = await response.arrayBuffer();
    const query = `?filename=${encodeURIComponent(name)}&source=${encodeURIComponent(source)}`;
    const saved = await fetch(endpoint('/add', query), {
      method: 'POST',
      headers: { 'Content-Type': 'application/octet-stream' },
      body: bytes,
    });
    const result = await saved.json();
    if (result.ok) notify(`Saved to Trove: ${name}`);
    else notify(`Trove refused: ${result.error || 'unknown error'}`);
  } catch (error) {
    notify(`Could not reach Trove (is it running?): ${error.message}`);
  }
});
