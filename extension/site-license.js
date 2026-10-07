// What the captured page (and the image's host) tell us about licensing.
//
// Three signals, strongest first:
//   1. a known site's own terms (Unsplash grants commercial use, 微博 does
//      not) — a static table, because these terms change at the site's pace,
//      not ours;
//   2. a CC license link in the page itself (Flickr, Wikimedia, Openverse
//      detail pages all carry one);
//   3. the image URL's host when the page is unrecognized — a forum
//      hot-linking a sinaimg.cn file is still pointing at 微博.
//
// The result is a hint, not a verdict. No table can clear an image for
// commercial use: model releases, trademarks and the site's current terms are
// all outside what a browser can see. The UI labels every tag as a reference.
//
// Shared by the background page (save toasts) and the picker panel (a source
// banner plus per-row tags). The CC page scan needs a DOM, so it silently
// returns nothing where `document` does not exist (the service worker).
(globalThis.TroveLicense = (() => {
  // kind: free — site license allows commercial use; public — public domain;
  // cc — licensed per item (look for a CC link in the page); buy — paid stock;
  // reserved — user uploads, rights stay with the author; unknown — cannot say.
  // Order matters where hosts nest (weixin.qq.com before qq.com): the first
  // match wins.
  const SITES = [
    // 免费可商用图库
    { label: 'Unsplash', kind: 'free', hosts: ['unsplash.com'] },
    { label: 'Pexels', kind: 'free', hosts: ['pexels.com'] },
    { label: 'Pixabay', kind: 'free', hosts: ['pixabay.com'] },
    { label: 'PxHere', kind: 'free', hosts: ['pxhere.com'] },
    { label: 'StockSnap', kind: 'free', hosts: ['stocksnap.io'] },
    { label: 'Burst', kind: 'free', hosts: ['burst.shopify.com'] },
    // 公共领域
    { label: 'NASA', kind: 'public', hosts: ['nasa.gov'] },
    { label: '大都会博物馆', kind: 'public', hosts: ['metmuseum.org'] },
    { label: '史密森尼', kind: 'public', hosts: ['si.edu'] },
    { label: '荷兰国立博物馆', kind: 'public', hosts: ['rijksmuseum.nl'] },
    // 逐张授权（CC / 公共领域混排）
    { label: 'Wikimedia', kind: 'cc', hosts: ['wikimedia.org', 'wikipedia.org'] },
    { label: 'Flickr', kind: 'cc', hosts: ['flickr.com', 'staticflickr.com', 'flic.kr'] },
    { label: 'Openverse', kind: 'cc', hosts: ['openverse.org'] },
    { label: 'Europeana', kind: 'cc', hosts: ['europeana.eu'] },
    // 商业图库
    { label: 'Getty Images', kind: 'buy', hosts: ['gettyimages.com', 'gettyimages.cn'] },
    { label: 'Shutterstock', kind: 'buy', hosts: ['shutterstock.com'] },
    { label: 'iStock', kind: 'buy', hosts: ['istockphoto.com'] },
    { label: 'Adobe Stock', kind: 'buy', hosts: ['stock.adobe.com'] },
    { label: 'Dreamstime', kind: 'buy', hosts: ['dreamstime.com'] },
    { label: 'Alamy', kind: 'buy', hosts: ['alamy.com'] },
    { label: '123RF', kind: 'buy', hosts: ['123rf.com'] },
    { label: '视觉中国', kind: 'buy', hosts: ['vcg.com'] },
    { label: '全景网', kind: 'buy', hosts: ['quanjing.com'] },
    // 国内社区（用户上传）
    { label: '微博', kind: 'reserved', hosts: ['weibo.com', 'weibo.cn', 'sinaimg.cn'] },
    { label: '知乎', kind: 'reserved', hosts: ['zhihu.com', 'zhimg.com'] },
    { label: '哔哩哔哩', kind: 'reserved', hosts: ['bilibili.com', 'b23.tv', 'hdslb.com', 'biliapi.net'] },
    { label: '小红书', kind: 'reserved', hosts: ['xiaohongshu.com', 'xhscdn.com', 'xhslink.com'] },
    { label: '抖音', kind: 'reserved', hosts: ['douyin.com', 'douyinpic.com', 'douyinstatic.com'] },
    { label: 'TikTok', kind: 'reserved', hosts: ['tiktok.com', 'tiktokcdn.com'] },
    { label: '微信公众号', kind: 'reserved', hosts: ['weixin.qq.com', 'qpic.cn', 'qlogo.cn'] },
    { label: '花瓣', kind: 'reserved', hosts: ['huaban.com', 'huabanimg.com'] },
    { label: 'LOFTER', kind: 'reserved', hosts: ['lofter.com'] },
    { label: '图虫', kind: 'reserved', hosts: ['tuchong.com'] },
    { label: '站酷', kind: 'reserved', hosts: ['zcool.com.cn', 'zcool.cn'] },
    { label: '豆瓣', kind: 'reserved', hosts: ['douban.com', 'doubanio.com'] },
    { label: '快手', kind: 'reserved', hosts: ['kuaishou.com', 'kwimgs.com'] },
    { label: '今日头条', kind: 'reserved', hosts: ['toutiao.com', 'toutiaoimg.com'] },
    // 国际社区（用户上传）
    { label: 'Instagram', kind: 'reserved', hosts: ['instagram.com', 'cdninstagram.com'] },
    { label: 'Facebook', kind: 'reserved', hosts: ['facebook.com', 'fbcdn.net'] },
    { label: 'X / Twitter', kind: 'reserved', hosts: ['x.com', 'twitter.com', 'twimg.com'] },
    { label: 'Pinterest', kind: 'reserved', hosts: ['pinterest.com', 'pinimg.com'] },
    { label: 'Reddit', kind: 'reserved', hosts: ['reddit.com', 'redd.it', 'redditmedia.com'] },
    { label: 'Tumblr', kind: 'reserved', hosts: ['tumblr.com'] },
    { label: 'YouTube', kind: 'reserved', hosts: ['youtube.com', 'youtu.be', 'ytimg.com', 'googlevideo.com'] },
    { label: 'Vimeo', kind: 'reserved', hosts: ['vimeo.com', 'vimeocdn.com'] },
    { label: 'Dribbble', kind: 'reserved', hosts: ['dribbble.com'] },
    { label: 'Behance', kind: 'reserved', hosts: ['behance.net'] },
    { label: 'ArtStation', kind: 'reserved', hosts: ['artstation.com'] },
    // 搜索与聚合：图片来自第三方，授权看原站
    { label: 'Google', kind: 'unknown', hosts: ['google.com', 'googleusercontent.com', 'gstatic.com'] },
    { label: 'Bing', kind: 'unknown', hosts: ['bing.com', 'bing.net'] },
    { label: '百度', kind: 'unknown', hosts: ['baidu.com', 'bdimg.com', 'bdstatic.com'] },
    { label: '搜狗', kind: 'unknown', hosts: ['sogou.com', 'sogoucdn.com'] },
    { label: '360 搜索', kind: 'unknown', hosts: ['so.com'] },
    // 门户：编辑内容与供稿图混排，一律说不准
    { label: '新浪', kind: 'unknown', hosts: ['sina.com.cn'] },
    { label: '搜狐', kind: 'unknown', hosts: ['sohu.com', 'itc.cn'] },
    { label: '网易', kind: 'unknown', hosts: ['163.com', 'netease.com'] },
    { label: '腾讯', kind: 'unknown', hosts: ['qq.com'] },
  ];

  const KIND_TAG = {
    free: '可商用',
    public: '公共领域',
    buy: '需购买授权',
    reserved: '不可商用',
    unknown: '暂无法判断',
  };
  const KIND_TONE = { free: 'good', public: 'good', buy: 'warn', reserved: 'bad', unknown: 'gray' };
  const KIND_NOTE = {
    free: '站方许可免费商用，具体以站方条款为准（如不得用于商标、需注意肖像权）',
    public: '公共领域或等效开放授权，可自由使用（含商用）',
    buy: '商业图库素材，需购买授权后使用',
    reserved: '用户上传内容，版权归作者，商用需取得作者授权',
    unknown: '无法自动判断版权，请到图片原站确认授权',
  };

  function hostOf(url) {
    try {
      return new URL(url).hostname.toLowerCase();
    } catch {
      return '';
    }
  }

  function siteFor(url) {
    const host = hostOf(url);
    if (!host) return null;
    for (const site of SITES) {
      for (const name of site.hosts) {
        if (host === name || host.endsWith(`.${name}`)) return site;
      }
    }
    return null;
  }

  // The license deed URL carries everything: /licenses/by-nc-sa/4.0/ or
  // /publicdomain/zero/1.0/. NC dominates (it forbids), then ND, then the
  // share-alike footnote.
  function parseCc(href) {
    if (!href) return null;
    const zero = href.match(/creativecommons\.org\/publicdomain\/zero\/([0-9.]+)/i);
    if (zero) return { tag: 'CC0 · 可商用', tone: 'good', note: `CC0 ${zero[1]}：作者已放弃版权，可自由使用（含商用）` };
    const mark = href.match(/creativecommons\.org\/publicdomain\/mark\/([0-9.]+)/i);
    if (mark) return { tag: '公共领域', tone: 'good', note: '公共领域标记（PDM）：已无已知版权限制' };
    const match = href.match(/creativecommons\.org\/licenses\/([a-z-]+)\/([0-9.]+)/i);
    if (!match) return null;
    const codes = match[1].toLowerCase();
    const name = `CC ${codes.toUpperCase()} ${match[2]}`;
    if (codes.split('-').includes('nc')) {
      return { tag: 'CC·禁商用', tone: 'bad', note: `${name}：含 NC（非商业性使用），不可商用` };
    }
    if (codes.split('-').includes('nd')) {
      return { tag: 'CC·禁改编', tone: 'warn', note: `${name}：可商用，但不得修改后再分发` };
    }
    return {
      tag: 'CC·需署名',
      tone: 'good',
      note: `${name}：可商用，须署名${codes.includes('sa') ? '并以相同方式共享' : ''}`,
    };
  }

  function ccFromDocument(doc = typeof document === 'undefined' ? null : document) {
    if (!doc?.querySelectorAll) return null;
    const nodes = doc.querySelectorAll(
      'link[rel~="license"], a[href*="creativecommons.org/licenses/"], a[href*="creativecommons.org/publicdomain/"]',
    );
    for (const node of nodes) {
      const cc = parseCc(node.href || node.getAttribute('href') || '');
      if (cc) return cc;
    }
    return null;
  }

  // The page-level read: which site it is, plus, for per-item sites, the CC
  // license found in the page (null in the service worker, where there is no
  // document to scan).
  function page(url) {
    const site = siteFor(url);
    if (!site) return null;
    return { site, cc: site.kind === 'cc' ? ccFromDocument() : null };
  }

  // The verdict for one save. With a recognized page every row shares it; on
  // an unrecognized page the image's own host may still name a site — but an
  // unknown-kind hit (a Bing thumbnail, say) is suppressed, because a gray
  // "cannot say" pill on every row is noise, not information.
  function row(pageInfo, itemUrl) {
    if (pageInfo) {
      if (pageInfo.cc) return { site: pageInfo.site.label, ...pageInfo.cc };
      const kind = pageInfo.site.kind;
      return {
        site: pageInfo.site.label,
        tag: kind === 'cc' ? '逐张确认' : KIND_TAG[kind],
        tone: kind === 'cc' ? 'warn' : KIND_TONE[kind],
        note: kind === 'cc' ? '本站内容逐张授权（多为 CC 或公共领域），请查看具体图片的授权说明' : KIND_NOTE[kind],
      };
    }
    const site = siteFor(itemUrl);
    if (!site || site.kind === 'unknown') return null;
    return { site: site.label, tag: KIND_TAG[site.kind], tone: KIND_TONE[site.kind], note: KIND_NOTE[site.kind] };
  }

  // Notification suffix for the single-save paths; null keeps the toast clean
  // when nothing is known.
  function short(pageUrl, itemUrl) {
    const verdict = row(page(pageUrl), itemUrl);
    return verdict ? `${verdict.site} · ${verdict.tag}` : null;
  }

  return { page, row, short, siteFor, parseCc, ccFromDocument, SITES };
})());
