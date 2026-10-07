// Preferences, shared by the service worker and the content scripts.
//
// They live in `chrome.storage.sync` so a second browser profile on the same
// account sees them; `port` stays in `storage.local` because it describes this
// machine's Trove, not this user's habits.
//
// Every default is on: the three switches each turn off something the extension
// already did before there were switches, and an unconfigured install should
// behave like the old one.
(globalThis.TrovePrefs = (() => {
  // Trove listens here unless the global `collect_port` setting says otherwise.
  // One definition, because a port the popup stored and the worker guessed
  // differently is a "Trove 未运行" that is nobody's fault.
  const DEFAULT_PORT = 23916;

  const DEFAULTS = {
    notifications: true,
    focusAfterSave: true,
    dragMenu: true,
  };

  const CACHE_TTL = 2000;
  let cache = null;
  let cachedAt = 0;

  // A dropped `sync` write (offline, quota) must not turn into a failed save:
  // the preference that did not persist is not the one being used right now.
  async function all() {
    if (cache && Date.now() - cachedAt < CACHE_TTL) return cache;
    let stored = {};
    try {
      stored = await chrome.storage.sync.get(Object.keys(DEFAULTS));
    } catch {
      stored = {};
    }
    cache = { ...DEFAULTS };
    for (const key of Object.keys(DEFAULTS)) {
      if (typeof stored[key] === 'boolean') cache[key] = stored[key];
    }
    cachedAt = Date.now();
    return cache;
  }

  async function get(key) {
    return (await all())[key];
  }

  async function set(key, value) {
    cache = null;
    try {
      await chrome.storage.sync.set({ [key]: value });
    } catch {
      // Report it to the options page, which is the only caller that can
      // still do something about it.
      return false;
    }
    return true;
  }

  async function port() {
    let stored;
    try {
      ({ port: stored } = await chrome.storage.local.get('port'));
    } catch {
      return DEFAULT_PORT;
    }
    return Number(stored) || DEFAULT_PORT;
  }

  // Drop the memoised copy (the options page after a write, a listener on
  // `chrome.storage.onChanged`).
  function invalidate() {
    cache = null;
  }

  return { DEFAULT_PORT, DEFAULTS, all, get, set, port, invalidate };
})());
