// The collection menu that opens while the user is dragging media.
//
// Two rules shape it, and both come from the fact that a drag owns the pointer:
//
//   - nothing here can be clicked. The panel is `pointer-events: none`, and the
//     content script feeds it raw drag coordinates, because the moment the
//     extension swallowed the pointer the drag would end. Row geometry is
//     therefore measured once per render and hit-tested by hand.
//   - navigating *into* a folder and *saving into* it are different gestures:
//     hovering a row for DWELL_MS drills, releasing on it saves. Without the
//     dwell a drag across a tree would save on the first row it brushed past.
(globalThis.TroveMenu = (() => {
  const DWELL_MS = 450;
  const MAX_ROWS = 40;
  const STYLE = `
    :host { all: initial; }
    .menu {
      position: fixed; z-index: 2147483646; pointer-events: none;
      font: 13px/1.45 system-ui, -apple-system, "Segoe UI", sans-serif;
      color: #f5f5f5; background: rgba(18, 18, 18, .96);
      border: 1px solid rgba(255, 255, 255, .15); border-radius: 10px;
      padding: 6px; min-width: 220px; max-width: 320px;
      max-height: 60vh; overflow: hidden;
      box-shadow: 0 10px 40px rgba(0, 0, 0, .45);
    }
    .head { color: #9a9a9a; padding: 4px 8px 6px; border-bottom: 1px solid rgba(255,255,255,.1); }
    .head b { color: #e6e6e6; font-weight: 600; }
    .path { color: #7fb3ff; }
    .row {
      display: flex; align-items: center; gap: 8px;
      padding: 5px 8px; border-radius: 6px; white-space: nowrap;
    }
    .row .name { flex: 1; overflow: hidden; text-overflow: ellipsis; }
    .row .count { color: #777; font-variant-numeric: tabular-nums; }
    .row .mark { color: #8b8b8b; width: 1em; text-align: right; }
    .row.hot { background: #2563eb; }
    .row.hot .count, .row.hot .mark { color: #dbeafe; }
    .row.dim { color: #8b8b8b; }
    .empty { color: #8b8b8b; padding: 6px 8px; }
  `;

  function rowEl(kind, label, extra = {}) {
    return { kind, label, id: extra.id ?? null, node: extra.node ?? null, count: extra.count };
  }

  function open({ x, y, library, rows, recents = [] }) {
    const host = document.createElement('div');
    host.setAttribute('data-trove-drag-menu', '');
    const shadow = host.attachShadow({ mode: 'open' });
    const style = document.createElement('style');
    style.textContent = STYLE;
    const menu = document.createElement('div');
    menu.className = 'menu';
    shadow.append(style, menu);
    (document.body || document.documentElement).append(host);

    const roots = TroveCollections.tree(rows);
    const byId = new Map((rows || []).map((row) => [row.id, row]));
    const recentNodes = recents.map((id) => byId.get(id)).filter(Boolean);
    let trail = [];
    let hot = -1;
    let dwell = null;
    let rects = [];
    let list = [];

    function place(px, py) {
      const box = menu.getBoundingClientRect();
      const left = Math.min(px + 18, Math.max(8, window.innerWidth - box.width - 8));
      const top = Math.min(py + 10, Math.max(8, window.innerHeight - box.height - 8));
      menu.style.left = `${left}px`;
      menu.style.top = `${top}px`;
    }

    function currentLevel() {
      return trail.length ? trail[trail.length - 1].children : roots;
    }

    function render() {
      const at = currentLevel();
      list = [];
      if (trail.length) list.push(rowEl('back', `‹ ${trail[trail.length - 1].name}`));
      if (!trail.length) {
        list.push(rowEl('default', '默认位置（不指定合集）'));
        if (recentNodes.length) {
          for (const node of recentNodes.slice(0, 4)) {
            list.push(rowEl('save', node.path, { id: node.id, node, count: node.assetCount }));
          }
        }
      }
      for (const node of at.slice(0, MAX_ROWS)) {
        list.push(
          rowEl(node.children.length ? 'drill' : 'save', node.name, {
            id: node.id,
            node,
            count: node.assetCount,
          }),
        );
      }

      menu.textContent = '';
      const head = document.createElement('div');
      head.className = 'head';
      const where = trail.length ? trail.map((n) => n.name).join(' / ') : library || 'Trove';
      head.innerHTML = `<b>保存到 Trove</b><br><span class="path"></span>`;
      head.querySelector('.path').textContent = where;
      menu.append(head);

      if (!list.filter((r) => r.kind !== 'back' && r.kind !== 'default').length) {
        const empty = document.createElement('div');
        empty.className = 'empty';
        empty.textContent = trail.length ? '这个合集里没有子合集' : '这个库还没有合集';
        menu.append(empty);
      }

      for (const [index, entry] of list.entries()) {
        const row = document.createElement('div');
        row.className = `row ${entry.kind === 'back' ? 'dim' : ''} ${index === hot ? 'hot' : ''}`;
        const name = document.createElement('span');
        name.className = 'name';
        name.textContent = entry.label;
        const count = document.createElement('span');
        count.className = 'count';
        count.textContent = entry.count == null ? '' : String(entry.count);
        const mark = document.createElement('span');
        mark.className = 'mark';
        mark.textContent = entry.kind === 'drill' ? '›' : entry.kind === 'back' ? '‹' : '';
        row.append(name, count, mark);
        menu.append(row);
      }

      // Placed first, then measured: a row's rectangle is the only thing that
      // says what is under the cursor during a drag, and measuring before the
      // panel is positioned records where it was not.
      place(x, y);
      rects = [...menu.querySelectorAll('.row')].map((row) => row.getBoundingClientRect());
    }

    function rowAt(px, py) {
      return rects.findIndex((r) => px >= r.left && px <= r.right && py >= r.top && py <= r.bottom);
    }

    function drill(entry) {
      if (entry.kind === 'back') {
        trail.pop();
        hot = -1;
        render();
        return;
      }
      if (entry.node?.children?.length) {
        trail.push(entry.node);
        hot = -1;
        render();
      }
    }

    function moveTo(px, py) {
      const index = rowAt(px, py);
      if (index === hot) return;
      clearTimeout(dwell);
      dwell = null;
      if (index >= 0) {
        const entry = list[index];
        if (entry.kind === 'drill' || entry.kind === 'back') {
          dwell = setTimeout(() => drill(entry), DWELL_MS);
        }
      }
      hot = index;
      render();
    }

    // Where the save goes: a row means that collection (the default row means
    // no destination), anything else means the drag ended outside the menu and
    // nothing was asked for.
    function pickAt(px, py) {
      const index = rowAt(px, py);
      if (index < 0) return undefined;
      const entry = list[index];
      if (entry.kind === 'back' || entry.kind === 'drill') return undefined;
      if (entry.kind === 'default') return null;
      return entry.id;
    }

    function close() {
      clearTimeout(dwell);
      host.remove();
    }

    render();
    return { moveTo, pickAt, close, contains: (px, py) => rowAt(px, py) >= 0 };
  }

  return { open };
})());
