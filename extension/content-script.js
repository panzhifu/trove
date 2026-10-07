// Drag media anywhere on a page and a Trove menu opens beside the cursor:
// hover a collection to step into it, release on one to save it there.
//
// The drag itself is never cancelled — `dragstart` is observed, not
// intercepted, so a site's own drag behaviour (a lightbox, a reorder, a native
// drop target) still runs. Only while the menu is open does this take over the
// drop, and only to keep the browser from navigating to the image instead.

let dragMenuEnabled = true;
TrovePrefs.get('dragMenu').then((value) => {
  dragMenuEnabled = value;
});
chrome.storage.onChanged.addListener((changes, area) => {
  if (area !== 'sync' || !('dragMenu' in changes)) return;
  dragMenuEnabled = changes.dragMenu.newValue !== false;
});

let menu = null;
let payload = null;
let dragging = false;
let rowsById = new Map();

function ask(message) {
  return new Promise((resolve) => {
    try {
      chrome.runtime.sendMessage(message, (reply) => {
        // A worker that went to sleep mid-save answers with lastError, not a
        // rejection; either way the caller needs a failure it can show.
        resolve(chrome.runtime.lastError ? { ok: false, error: chrome.runtime.lastError.message } : reply);
      });
    } catch (error) {
      resolve({ ok: false, error: error.message });
    }
  });
}

function teardown() {
  dragging = false;
  menu?.close();
  menu = null;
  payload = null;
  window.removeEventListener('dragover', onDragOver, true);
  window.removeEventListener('drop', onDrop, true);
  window.removeEventListener('dragend', onDragEnd, true);
  document.removeEventListener('keydown', onKeyDown, true);
}

function onKeyDown(event) {
  if (event.key === 'Escape' && menu) teardown();
}

async function onDragStart(event) {
  if (!dragMenuEnabled || menu || event.defaultPrevented) return;
  const media = TroveMedia.fromDragEvent(event);
  if (!media) return;

  // The coordinates and the "a drag is in flight" flag are both taken now,
  // before anything is awaited: asking the worker for the tree is a round
  // trip, and a drag that ended during it must not open a menu nobody can
  // close. `dragend` is registered first for the same reason — it has to be
  // listening even if the menu never appears.
  const x = event.clientX;
  const y = event.clientY;
  dragging = true;
  window.addEventListener('dragend', onDragEnd, true);

  const reply = await ask({ type: 'trove-status' });
  if (!Array.isArray(reply?.collections)) {
    TroveBubble.show(
      x,
      y,
      reply?.connected === false ? 'Trove 未运行，无法选择合集' : '读取合集列表失败',
      'bad',
    );
    teardown();
    return;
  }
  if (!dragging) return;

  rowsById = new Map(reply.collections.map((row) => [row.id, row]));
  payload = media;
  menu = TroveMenu.open({
    x,
    y,
    library: reply.library,
    rows: reply.collections,
    recents: await TroveCollections.recentIds(reply.collections.map((row) => row.id)),
  });
  window.addEventListener('dragover', onDragOver, true);
  window.addEventListener('drop', onDrop, true);
  document.addEventListener('keydown', onKeyDown, true);
  // Released while the recents list was being read: the menu just opened into
  // a drag that is already over.
  if (!dragging) teardown();
}

function onDragOver(event) {
  if (!menu) return;
  // Without this the browser never delivers a drop, and the menu would have
  // nothing to be released on.
  event.preventDefault();
  menu.moveTo(event.clientX, event.clientY);
}

function onDragEnd() {
  teardown();
}

async function onDrop(event) {
  if (!menu) return;
  event.preventDefault();
  event.stopPropagation();
  // Read before the menu is torn down: the coordinates mean nothing afterwards.
  const target = menu.pickAt(event.clientX, event.clientY);
  const media = payload;
  const x = event.clientX;
  const y = event.clientY;
  teardown();
  // `undefined` is a release outside the menu — the user changed their mind,
  // which is neither a save nor an error. `null` is the default destination.
  if (target === undefined || !media) return;
  await save(media, target, x, y);
}

async function save(media, collection, x, y) {
  const where = collection ? rowsById.get(collection)?.path : null;
  TroveBubble.show(x, y, collection ? `正在保存到「${where || '合集'}」…` : '正在保存到 Trove…');
  const reply = await ask({
    type: 'trove-drag-save',
    url: media.url,
    source: document.URL,
    collection: collection || null,
  });
  if (reply?.ok) {
    TroveBubble.show(x, y, collection ? `已保存到「${where}」：${reply.file}` : `已保存：${reply.file}`, 'ok');
  } else {
    TroveBubble.show(x, y, `保存失败：${reply?.error || '未知错误'}`, 'bad');
  }
}

document.addEventListener('dragstart', onDragStart, true);
