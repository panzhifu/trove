// The capsule that says what happened to a save, drawn where the user asked
// for it. It exists because a system notification is easy to miss (and is
// switchable off), while a save that vanished silently is worse than one that
// failed loudly.
//
// `pointer-events: none` is load-bearing: during a drag the cursor must keep
// hitting the menu behind this, not the capsule.
(globalThis.TroveBubble = (() => {
  const HOST_ID = 'trove-save-bubble';
  const LIFE = 2200;
  let host = null;
  let hideTimer = null;

  const STYLE = `
    :host { all: initial; }
    .pill {
      position: fixed; z-index: 2147483647; pointer-events: none;
      transform: translate(-50%, 0);
      font: 13px/1.4 system-ui, -apple-system, "Segoe UI", sans-serif;
      color: #f5f5f5; background: rgba(20, 20, 20, .92);
      border: 1px solid rgba(255, 255, 255, .14); border-radius: 999px;
      padding: 6px 14px; max-width: min(46vw, 420px);
      white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
      box-shadow: 0 6px 24px rgba(0, 0, 0, .35);
      opacity: 0; transition: opacity .18s ease;
    }
    .pill.on { opacity: 1; }
    .pill.ok { border-color: rgba(74, 222, 128, .5); }
    .pill.bad { border-color: rgba(248, 113, 113, .55); }
  `;

  function mount() {
    if (host && host.isConnected) return host;
    host = document.createElement('div');
    host.id = HOST_ID;
    const shadow = host.attachShadow({ mode: 'open' });
    const style = document.createElement('style');
    style.textContent = STYLE;
    const pill = document.createElement('div');
    pill.className = 'pill';
    shadow.append(style, pill);
    (document.body || document.documentElement).append(host);
    return host;
  }

  // x/y are viewport pixels; the pill is clamped so a drag near an edge does
  // not push its text off screen.
  function show(x, y, text, tone = 'info') {
    const node = mount();
    const pill = node.shadowRoot.querySelector('.pill');
    pill.textContent = text;
    pill.className = `pill ${tone}`;
    pill.style.left = `${Math.min(Math.max(x, 170), Math.max(window.innerWidth - 170, 170))}px`;
    pill.style.top = `${Math.min(Math.max(y, 8), window.innerHeight - 44)}px`;
    requestAnimationFrame(() => pill.classList.add('on'));
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => pill.classList.remove('on'), LIFE);
  }

  return { show };
})());
