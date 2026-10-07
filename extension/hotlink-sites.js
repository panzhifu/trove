// Sites that refuse an image request unless the Referer names *their own*
// origin. A captured page's URL is the right Referer for most hosts and
// useless for these, so the list gives each one the referer it checks for.
//
// Two paths use it, and they are not interchangeable:
//   - the browser-relay path cannot set Referer from a service worker at all
//     (`fetch` forbids the header), so on Chromium a dynamic
//     `declarativeNetRequest` rule rewrites it in the network stack;
//   - the server-side `/fetch` path takes the referer as an argument, so it
//     works in every browser — which is why Firefox still saves from these
//     sites with no rules installed, just via the slower path.
(globalThis.TroveHotlink = (() => {
  const SITES = [
    { label: '微博', hosts: ['sinaimg.cn'], referer: 'https://weibo.com/' },
    { label: '知乎', hosts: ['zhimg.com'], referer: 'https://www.zhihu.com/' },
    { label: '哔哩哔哩', hosts: ['hdslb.com'], referer: 'https://www.bilibili.com/' },
    { label: '搜狐', hosts: ['itc.cn'], referer: 'https://www.sohu.com/' },
  ];

  // Rule ids are ours from this base up: everything at or above it is recycled
  // on every install, so a table edit cannot leave a stale rule behind.
  const RULE_BASE = 200000;

  function entryFor(url) {
    let host;
    try {
      host = new URL(url).hostname.toLowerCase();
    } catch {
      return null;
    }
    return SITES.find((site) =>
      site.hosts.some((name) => host === name || host.endsWith(`.${name}`)),
    ) || null;
  }

  // The site's own origin when the table knows it, `fallback` (the capturing
  // page) otherwise.
  function refererFor(url, fallback) {
    return entryFor(url)?.referer || fallback;
  }

  async function installRules() {
    const dnr = chrome.declarativeNetRequest;
    if (!dnr?.updateDynamicRules) return false;
    const existing = await dnr.getDynamicRules().catch(() => []);
    const removeRuleIds = existing.map((rule) => rule.ruleId);
    const addRules = SITES.flatMap((site, index) =>
      site.hosts.map((name, hostIndex) => ({
        ruleId: RULE_BASE + index * 16 + hostIndex,
        priority: 1,
        action: {
          type: 'modifyHeaders',
          requestHeaders: [
            { header: 'referer', operation: 'set', value: site.referer },
          ],
        },
        condition: {
          urlFilter: `||${name}`,
          resourceTypes: ['xmlhttprequest', 'image'],
        },
      })),
    );
    try {
      await dnr.updateDynamicRules({ removeRuleIds, addRules });
      return true;
    } catch {
      // Firefox's implementation of this API rejects header edits. Nothing is
      // lost: `/fetch` carries the referer itself.
      return false;
    }
  }

  return { SITES, entryFor, refererFor, installRules };
})());
