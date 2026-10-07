// What a drag is actually carrying, as a URL worth saving.
//
// Three sources, in the order a browser makes them trustworthy: the element
// under the cursor (the only one that knows a srcset's real file), the
// DataTransfer payload pages put on the drag themselves, and finally whatever
// image is under the point the drag began. A drag started on a link that points
// at a .jpg still counts — that is how a "大图" link behaves on photo sites.
(globalThis.TroveMedia = (() => {
  const MEDIA_EXT =
    /\.(png|jpe?g|gif|webp|avif|bmp|svg|ico|tif?f|heic|heif|apng|mp4|webm|mov|m4v|mkv|avi)([?#]|$)/i;
  const ANCESTOR_HOPS = 8;

  // Extension-shaped, because that is all a URL says before anything has been
  // fetched. Sources that already know what they are (`<img>`, `<video>`, a CSS
  // background) do not have to pass this.
  function isMediaUrl(url) {
    if (!url) return false;
    if (url.startsWith('data:image/') || url.startsWith('blob:')) return true;
    return /^https?:\/\//i.test(url) && MEDIA_EXT.test(url.split('#')[0]);
  }

  function kindOf(url, fromElement) {
    if (fromElement === 'video') return 'video';
    if (fromElement === 'image') return 'image';
    return /\.(mp4|webm|mov|m4v|mkv|avi)([?#]|$)/i.test(url || '') ? 'video' : 'image';
  }

  // An `<img>` already resolved its srcset; `currentSrc` is the file it chose,
  // `src` is only the fallback the attribute was written with.
  function fromImage(el) {
    return el?.currentSrc || el?.src || null;
  }

  function fromVideo(el) {
    if (el?.currentSrc) return el.currentSrc;
    const source = el?.querySelector?.('source[src]');
    return source?.src || el?.src || null;
  }

  // A CSS background image: the URL in the declaration, resolved against the
  // page so a site-relative path becomes something fetchable.
  function fromBackground(el) {
    const value = window.getComputedStyle(el).backgroundImage;
    if (!value || value === 'none') return null;
    const match = value.match(/url\(["']?([^"')]+)["']?\)/);
    if (!match) return null;
    try {
      return new URL(match[1], document.baseURI).href;
    } catch {
      return null;
    }
  }

  // Nearest media in the element's own subtree, then up its ancestors: a card
  // is usually dragged from a wrapper `<a>` or `<div>`, not from the `<img>`.
  function fromElement(target) {
    let el = target;
    for (let hop = 0; el && hop <= ANCESTOR_HOPS; hop += 1, el = el.parentElement) {
      if (el.tagName === 'IMG') {
        const url = fromImage(el);
        if (url) return { url, from: 'image' };
      }
      if (el.tagName === 'VIDEO') {
        const url = fromVideo(el);
        if (url) return { url, from: 'video' };
      }
      if (el.tagName === 'SOURCE' && el.parentElement?.tagName === 'VIDEO') {
        const url = el.src && new URL(el.src, document.baseURI).href;
        if (url) return { url, from: 'video' };
      }
      const background = fromBackground(el);
      if (background && isMediaUrl(background)) return { url: background, from: 'image' };
      if (hop === 0) {
        const inside = el.querySelector?.('img,picture img,svg image,video');
        if (inside) {
          const url = fromImage(inside) || fromVideo(inside);
          if (url) return { url, from: inside.tagName === 'VIDEO' ? 'video' : 'image' };
        }
      }
    }
    return null;
  }

  // What the page chose to put on the drag. Only accepted when it looks like a
  // media file: a text selection dragged out of an article is not a capture.
  function fromDataTransfer(data) {
    if (!data) return null;
    const uriList = data.getData('text/uri-list');
    if (uriList) {
      const line = uriList.split(/\r?\n/).find((entry) => entry && !entry.startsWith('#'));
      if (line && isMediaUrl(line)) return { url: line, from: 'transfer' };
    }
    const html = data.getData('text/html');
    if (html) {
      const doc = new DOMParser().parseFromString(html, 'text/html');
      const img = doc.querySelector('img[src]');
      if (img?.src && isMediaUrl(img.src)) return { url: img.src, from: 'image' };
      const anchor = doc.querySelector('a[href]');
      if (anchor?.href && isMediaUrl(anchor.href)) return { url: anchor.href, from: 'transfer' };
    }
    const text = data.getData('text/plain');
    if (text && isMediaUrl(text)) return { url: text, from: 'transfer' };
    return null;
  }

  // The drag ghost carries no element at all on some sites (canvas apps,
  // image boards that redraw tiles): take the largest image under the point.
  function fromPoint(x, y) {
    const hits = document.elementsFromPoint(x, y).slice(0, ANCESTOR_HOPS);
    let best = null;
    for (const el of hits) {
      if (el.tagName === 'IMG' || el.tagName === 'VIDEO') {
        const url = fromImage(el) || fromVideo(el);
        if (!url) continue;
        const area = el.getBoundingClientRect().width * el.getBoundingClientRect().height;
        if (!best || area > best.area) best = { url, from: el.tagName === 'VIDEO' ? 'video' : 'image', area };
      }
    }
    return best;
  }

  function fromDragEvent(event) {
    const target = event.composedPath?.()[0] || event.target;
    const found =
      (target && fromElement(target)) ||
      fromDataTransfer(event.dataTransfer) ||
      fromPoint(event.clientX, event.clientY);
    if (!found) return null;
    let url = found.url;
    try {
      url = new URL(url, document.baseURI).href;
    } catch {
      return null;
    }
    if (!/^(https?|data|blob):/i.test(url)) return null;
    // A same-page anchor drag whose URL is the page itself is not a capture.
    if (found.from !== 'image' && found.from !== 'video' && !isMediaUrl(url)) return null;
    return { url, kind: kindOf(url, found.from) };
  }

  return { fromDragEvent, isMediaUrl };
})());
