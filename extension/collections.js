// The collection tree the save menu is built from, and the destinations the
// user picked recently.
//
// A failed `/collections` is `null` rather than an empty list, because the two
// mean different things: "Trove could not tell us" must fall back to saving
// where saves normally go, while "this library has no collections" is a real
// answer and simply leaves the menu with nothing in it.
(globalThis.TroveCollections = (() => {
  const RECENT_KEY = 'recentCollections';
  const RECENT_MAX = 20;
  const RECENT_PICK = 4;
  const LOAD_TIMEOUT = 3000;

  async function load(port) {
    let response;
    try {
      response = await fetch(`http://127.0.0.1:${port}/collections`, {
        signal: AbortSignal.timeout(LOAD_TIMEOUT),
      });
    } catch {
      return null;
    }
    const json = await response.json().catch(() => null);
    if (!json || json.ok !== true || !Array.isArray(json.collections)) return null;
    return json;
  }

  // The service lists a parent before its children, so one pass with an id map
  // is the whole rebuild; a row whose parent is missing stays at the top
  // rather than vanishing with its subtree.
  function tree(collections) {
    const byId = new Map();
    const roots = [];
    for (const row of collections || []) {
      const node = { ...row, children: [] };
      byId.set(node.id, node);
      const parent = node.parentId ? byId.get(node.parentId) : null;
      if (parent) parent.children.push(node);
      else roots.push(node);
    }
    return roots;
  }

  async function storedRecents() {
    try {
      const { [RECENT_KEY]: ids } = await chrome.storage.local.get(RECENT_KEY);
      return Array.isArray(ids) ? ids : [];
    } catch {
      return [];
    }
  }

  // Ids still in the library, most recent first, capped for a menu: a
  // collection deleted since is dropped here rather than offered and refused
  // at save time.
  async function recentIds(liveIds) {
    const live = liveIds instanceof Set ? liveIds : new Set(liveIds || []);
    const ids = (await storedRecents()).filter((id) => live.has(id));
    return ids.slice(0, RECENT_MAX);
  }

  async function remember(id) {
    if (!id) return;
    const ids = [id, ...(await storedRecents()).filter((known) => known !== id)];
    try {
      await chrome.storage.local.set({ [RECENT_KEY]: ids.slice(0, RECENT_MAX) });
    } catch {
      // Losing the recent list is not worth failing a save over.
    }
  }

  return { load, tree, recentIds, remember, RECENT_PICK };
})());
