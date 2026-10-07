// The batch grabber: scan this document for its images and videos, let the
// user pick, and hand the chosen URLs to the worker one at a time — each item
// then rides the same save path as a right-click save (browser relay first,
// then the server-side /fetch fallback). No notification per item; the panel
// itself shows progress and marks each row ✓ or ✗.
//
// The scan is local and instant, so the panel opens before the worker answers:
// the destination list and the connection state fill in when the status reply
// arrives. A disconnected Trove only disables the save button — the list is
// about the page, not the service, and stays browsable.
//
// Unlike the drag menu this panel owns the pointer, so it is a real UI:
// checkboxes, a <select> for the destination, buttons. Esc or ✕ closes it,
// and the menu item and popup button are both toggles — asked for while open,
// the panel closes rather than stacking a second one.
(globalThis.TrovePicker = (() => {
  // An image smaller than this on its longest side is an icon or a spacer, not
  // content. Sizes unknown (lazy-loaded, never fetched) are kept, not dropped —
  // below-the-fold content is exactly what this feature exists for.
  const MIN_SIDE = 100;
  // One scrollable list, not a census: past this the biggest known images win.
  const MAX_ITEMS = 200;
  // data: URIs past this do not get decoded into a thumbnail either; the row
  // keeps a glyph instead of a several-hundred-kilobyte inline image.
  const THUMB_DATA_MAX = 128 * 1024;

  const STYLE = `
    :host { all: initial; }
    .panel, .panel * { box-sizing: border-box; }
    .panel {
      position: fixed; right: 20px; top: 50%; transform: translateY(-50%);
      z-index: 2147483647; width: 344px; max-height: 78vh;
      display: flex; flex-direction: column;
      font: 13px/1.45 system-ui, -apple-system, "Segoe UI", sans-serif;
      color: #f5f5f5; background: rgba(18, 18, 18, .97);
      border: 1px solid rgba(255, 255, 255, .15); border-radius: 10px;
      box-shadow: 0 10px 40px rgba(0, 0, 0, .45);
    }
    .head {
      display: flex; align-items: center; gap: 8px;
      padding: 9px 8px 8px 12px; border-bottom: 1px solid rgba(255,255,255,.1);
    }
    .head b { font-weight: 600; }
    .head .lib {
      flex: 1; min-width: 0; color: #7fb3ff; font-size: 12px;
      overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
    }
    .head .x { all: unset; cursor: pointer; color: #9a9a9a; padding: 2px 7px; border-radius: 6px; }
    .head .x:hover { color: #fff; background: rgba(255,255,255,.08); }
    .banner {
      display: flex; align-items: center; gap: 8px;
      padding: 6px 12px; color: #9a9a9a; cursor: help;
      border-bottom: 1px solid rgba(255,255,255,.06);
    }
    .banner .site-name { color: #e6e6e6; }
    .sub { display: flex; align-items: center; gap: 6px; min-width: 0; }
    .tag {
      flex-shrink: 0; font-size: 10px; line-height: 1.6;
      padding: 0 7px; border-radius: 999px; border: 1px solid;
    }
    .tag.good { color: #4ade80; border-color: rgba(74,222,128,.45); background: rgba(74,222,128,.08); }
    .tag.warn { color: #fbbf24; border-color: rgba(251,191,36,.45); background: rgba(251,191,36,.08); }
    .tag.bad { color: #f87171; border-color: rgba(248,113,113,.5); background: rgba(248,113,113,.08); }
    .tag.gray { color: #9a9a9a; border-color: rgba(255,255,255,.25); background: rgba(255,255,255,.05); }
    .bar { display: flex; align-items: center; gap: 10px; padding: 8px 12px 6px; color: #9a9a9a; }
    .bar .count { flex: 1; font-variant-numeric: tabular-nums; }
    .bar .mini { all: unset; cursor: pointer; color: #7fb3ff; font-size: 12px; padding: 2px 5px; border-radius: 4px; }
    .bar .mini:hover { background: rgba(255,255,255,.08); }
    .bar .mini:disabled { opacity: .4; cursor: default; }
    .list { flex: 1; min-height: 0; overflow-y: auto; padding: 4px 8px 8px; }
    .list.lock { pointer-events: none; opacity: .55; }
    .list::-webkit-scrollbar { width: 10px; }
    .list::-webkit-scrollbar-thumb { background: rgba(255,255,255,.15); border-radius: 5px; }
    .row { display: flex; align-items: center; gap: 10px; padding: 5px 6px; border-radius: 8px; cursor: pointer; }
    .row:hover { background: rgba(255,255,255,.06); }
    .row input { accent-color: #2563eb; flex-shrink: 0; margin: 0; }
    .thumb {
      width: 46px; height: 46px; border-radius: 6px; object-fit: cover;
      background: #242424; border: 1px solid rgba(255,255,255,.1); flex-shrink: 0;
    }
    .thumb.glyph { display: flex; align-items: center; justify-content: center; color: #777; font-size: 17px; }
    .meta { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 2px; }
    .name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .dims { color: #777; font-size: 11px; font-variant-numeric: tabular-nums; }
    .mark { width: 1.2em; text-align: center; flex-shrink: 0; }
    .mark.good { color: #4ade80; }
    .mark.bad { color: #f87171; }
    .row.fail .name { color: #f87171; }
    .empty { color: #8b8b8b; padding: 14px 10px; }
    .progress { padding: 0 12px; color: #9a9a9a; min-height: 20px; }
    .progress.ok { color: #4ade80; }
    .progress.bad { color: #f87171; }
    .foot {
      display: flex; align-items: center; gap: 8px;
      padding: 8px 12px 10px; border-top: 1px solid rgba(255,255,255,.1);
    }
    .foot select {
      flex: 1; min-width: 0; background: #1f1f1f; border: 1px solid #333;
      color: #eee; border-radius: 6px; padding: 5px 6px; font: inherit;
    }
    .btn { all: unset; cursor: pointer; border-radius: 6px; padding: 5px 12px; background: rgba(255,255,255,.08); }
    .btn:hover { background: rgba(255,255,255,.14); }
    .btn.primary { background: #2563eb; color: #fff; }
    .btn.primary:hover { background: #1d4ed8; }
    .btn:disabled { opacity: .45; cursor: default; }
  `;

  let panel = null;

  function el(tag, className) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    return node;
  }

  function glyph(char) {
    const div = el('div', 'thumb glyph');
    div.textContent = char;
    return div;
  }

  // The license tag as a pill; the hover title carries the actual explanation.
  function tagPill(verdict) {
    const pill = el('span', `tag ${verdict.tone}`);
    pill.textContent = verdict.tag;
    pill.title = verdict.note || '';
    return pill;
  }

  function fileNameFromUrl(url) {
    const segment = url.split(/[?#]/)[0].split('/').pop() || '';
    try {
      return decodeURIComponent(segment) || '（无文件名）';
    } catch {
      return segment || '（无文件名）';
    }
  }

  function absolute(raw) {
    try {
      return new URL(raw, document.baseURI || location.href).href;
    } catch {
      return null;
    }
  }

  // Everything in the document that names a fetchable image or video, once
  // each, biggest first. `currentSrc` is what the browser actually picked from
  // a srcset or <picture>; a bare srcset candidate on <source> is resolved
  // against the page. blob: sources are left out on purpose — video sites hand
  // out MediaSource blobs the save path cannot read.
  function scan() {
    const seen = new Set();
    const items = [];
    const add = (raw, kind, owner) => {
      if (!raw) return;
      const url = absolute(raw);
      if (!url || seen.has(url)) return;
      if (!/^https?:/i.test(url) && !url.startsWith('data:image/')) return;
      // Tiny inline images are icons, whatever their declared size says.
      if (url.startsWith('data:') && url.length < 4096) return;
      seen.add(url);
      const w = owner?.naturalWidth || owner?.videoWidth || 0;
      const h = owner?.naturalHeight || owner?.videoHeight || 0;
      const poster = owner?.poster || '';
      items.push({ url, kind, w, h, poster });
    };
    for (const img of document.querySelectorAll('img')) {
      add(img.currentSrc || img.getAttribute('src'), 'image', img);
    }
    for (const video of document.querySelectorAll('video')) {
      add(video.currentSrc || video.getAttribute('src'), 'video', video);
    }
    for (const source of document.querySelectorAll('picture source, video source')) {
      const first = (source.getAttribute('srcset') || '').split(',')[0].trim().split(/\s+/)[0];
      add(source.getAttribute('src') || first, source.closest('video') ? 'video' : 'image', null);
    }
    // Content first: measured area descending, unknown sizes sinking below
    // everything measured rather than floating to the top.
    return items
      .filter((it) => !(it.w && it.h) || Math.max(it.w, it.h) >= MIN_SIDE)
      .sort((a, b) => {
        const area = (it) => (it.w && it.h ? it.w * it.h : -1);
        return area(b) - area(a);
      })
      .slice(0, MAX_ITEMS);
  }

  // The panel only reads status; a dead worker shows up as "not connected",
  // never as an exception.
  function ask(message) {
    return new Promise((resolve) => {
      try {
        chrome.runtime.sendMessage(message, (reply) => {
          resolve(chrome.runtime.lastError ? null : reply);
        });
      } catch {
        resolve(null);
      }
    });
  }

  function openPanel() {
    const found = scan();
    const state = { saving: false, connected: false, closed: false };

    const host = document.createElement('div');
    host.setAttribute('data-trove-picker', '');
    const shadow = host.attachShadow({ mode: 'open' });
    const style = document.createElement('style');
    style.textContent = STYLE;

    const root = el('div', 'panel');

    const head = el('div', 'head');
    const title = el('b');
    title.textContent = '抓取本页媒体';
    const lib = el('span', 'lib');
    const closeBtn = el('button', 'x');
    closeBtn.textContent = '✕';
    closeBtn.title = '关闭（Esc）';
    head.append(title, lib, closeBtn);

    const bar = el('div', 'bar');
    const countEl = el('span', 'count');
    const allBtn = el('button', 'mini');
    allBtn.textContent = '全选';
    const noneBtn = el('button', 'mini');
    noneBtn.textContent = '清空';
    bar.append(countEl, allBtn, noneBtn);

    const list = el('div', 'list');
    const progress = el('div', 'progress');

    const foot = el('div', 'foot');
    const select = document.createElement('select');
    const defaultOpt = document.createElement('option');
    defaultOpt.value = '';
    defaultOpt.textContent = '默认位置（不指定合集）';
    select.append(defaultOpt);
    const saveBtn = el('button', 'btn primary');
    const cancelBtn = el('button', 'btn');
    cancelBtn.textContent = '取消';
    foot.append(select, saveBtn, cancelBtn);

    root.append(head, bar, list, progress, foot);
    shadow.append(style, root);
    (document.body || document.documentElement).append(host);

    // What the page's own site says about every item on it; recognized sites
    // get a banner and the rows stay clean.
    const license = typeof TroveLicense === 'undefined' ? null : TroveLicense;
    const pageInfo = license ? license.page(location.href) : null;
    const pageTag = license ? license.row(pageInfo, null) : null;
    if (pageTag) {
      const banner = el('div', 'banner');
      const siteName = el('span', 'site-name');
      siteName.textContent = `来源：${pageTag.site}`;
      banner.title = pageTag.note || '';
      banner.append(siteName, tagPill(pageTag));
      root.insertBefore(banner, bar);
    }

    if (!found.length) {
      const empty = el('div', 'empty');
      empty.textContent = '这一页没有找到可保存的图片或视频';
      list.append(empty);
    }

    const rows = found.map((item) => {
      const row = el('label', 'row');
      const check = document.createElement('input');
      check.type = 'checkbox';
      check.checked = true;
      let thumb;
      if (item.kind === 'video') {
        thumb = item.poster ? document.createElement('img') : glyph('▶');
        if (item.poster) thumb.src = item.poster;
      } else if (item.url.startsWith('data:') && item.url.length > THUMB_DATA_MAX) {
        thumb = glyph('🖼');
      } else {
        thumb = document.createElement('img');
        thumb.loading = 'lazy';
        thumb.onerror = () => {
          thumb.onerror = null;
          thumb.replaceWith(glyph('🖼'));
        };
        thumb.src = item.url;
      }
      thumb.className = 'thumb';
      const meta = el('span', 'meta');
      const name = el('span', 'name');
      name.textContent = fileNameFromUrl(item.url);
      name.title = item.url;
      const dims = el('span', 'dims');
      const size = item.w && item.h ? `${item.w}×${item.h}` : '尺寸未知';
      dims.textContent = item.kind === 'video' ? `视频 · ${size}` : size;
      const sub = el('span', 'sub');
      sub.append(dims);
      // An unrecognized page falls back to the image's own host (a forum
      // hot-linking a weibo CDN still hints at 微博); a recognized page keeps
      // its one banner and the rows stay clean.
      if (!pageInfo && license) {
        const rowTag = license.row(null, item.url);
        if (rowTag) sub.append(tagPill(rowTag));
      }
      const mark = el('span', 'mark');
      meta.append(name, sub);
      row.append(check, thumb, meta, mark);
      list.append(row);
      check.addEventListener('change', refresh);
      return { item, row, check, mark };
    });

    function refresh() {
      const chosen = rows.filter((r) => r.check.checked).length;
      countEl.textContent = `已选 ${chosen} / ${rows.length}`;
      saveBtn.textContent = state.saving ? '保存中…' : `保存 ${chosen} 项`;
      saveBtn.disabled = state.saving || !state.connected || !chosen;
      select.disabled = state.saving;
      allBtn.disabled = noneBtn.disabled = state.saving;
      list.classList.toggle('lock', state.saving);
    }

    function close() {
      state.closed = true;
      window.removeEventListener('keydown', onKey, true);
      host.remove();
      panel = null;
    }

    function onKey(event) {
      if (event.key !== 'Escape') return;
      // Esc belongs to whatever the user is typing into first: an open select
      // or a text field gets it, a checkbox or the page itself closes us.
      const t = event.target;
      const busy =
        t instanceof HTMLTextAreaElement ||
        t instanceof HTMLSelectElement ||
        t?.isContentEditable ||
        (t instanceof HTMLInputElement && t.type !== 'checkbox');
      if (busy) return;
      close();
    }

    window.addEventListener('keydown', onKey, true);
    closeBtn.addEventListener('click', close);
    cancelBtn.addEventListener('click', close);
    allBtn.addEventListener('click', () => {
      for (const r of rows) r.check.checked = true;
      refresh();
    });
    noneBtn.addEventListener('click', () => {
      for (const r of rows) r.check.checked = false;
      refresh();
    });

    async function saveAll() {
      const chosen = rows.filter((r) => r.check.checked);
      if (!chosen.length || state.saving) return;
      state.saving = true;
      refresh();
      let ok = 0;
      let failed = 0;
      for (const [index, r] of chosen.entries()) {
        if (state.closed) return;
        progress.className = 'progress';
        progress.textContent = `保存中 ${index + 1} / ${chosen.length}…`;
        const reply = await ask({
          type: 'trove-drag-save',
          url: r.item.url,
          source: location.href,
          collection: select.value || null,
        });
        if (state.closed) return;
        if (reply?.ok) {
          ok += 1;
          r.mark.textContent = '✓';
          r.mark.classList.add('good');
        } else {
          failed += 1;
          r.mark.textContent = '✗';
          r.mark.classList.add('bad');
          r.row.classList.add('fail');
        }
      }
      const where = select.selectedOptions[0]?.textContent;
      progress.className = failed ? 'progress bad' : 'progress ok';
      progress.textContent = failed
        ? `完成：${ok} 个成功，${failed} 个失败`
        : `完成：${ok} 个已保存${where ? `到「${where}」` : ''}`;
      state.saving = false;
      refresh();
    }
    saveBtn.addEventListener('click', saveAll);

    panel = { close };

    refresh();
    ask({ type: 'trove-status' }).then((reply) => {
      if (state.closed) return;
      if (reply?.connected) {
        state.connected = true;
        lib.textContent = reply.library || '';
        const cols = Array.isArray(reply.collections) ? reply.collections : [];
        const sorted = [...cols].sort((a, b) =>
          String(a.path).localeCompare(String(b.path), 'zh-Hans-CN'),
        );
        for (const col of sorted) {
          const opt = document.createElement('option');
          opt.value = col.id;
          opt.textContent = col.path || col.name || col.id;
          select.append(opt);
        }
      } else {
        progress.textContent = '未连接：请先启动 Trove 桌面应用，列表仍可浏览';
      }
      refresh();
    });
  }

  function toggle() {
    if (panel) {
      panel.close();
      return;
    }
    openPanel();
  }

  chrome.runtime.onMessage.addListener((message) => {
    if (message?.type === 'trove-grab-open') toggle();
    return false;
  });

  return { toggle, scan };
})());
