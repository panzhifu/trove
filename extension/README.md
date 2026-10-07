# Trove 采集器（浏览器扩展）

把网页上的图片、视频、链接指向的文件存进本地 Trove 素材库，并且存进你指定的合集。
通过 Trove 的本地采集服务（`http://127.0.0.1:23916`）上传，Trove 运行中即会自动导
入。来源 URL 与目标合集随文件写进同目录的 `<name>.meta.json` sidecar，导入时分别落
到资产的 `assets.source_url` 列和那个合集的成员表，随后删掉 sidecar（文件本身留在
收件目录，库是链接它）。

## 安装（Chrome / Edge）

1. 启动 Trove 桌面应用（采集服务默认开启，监听 `127.0.0.1:23916`）。Trove 侧没有端口
   UI：默认 23916，要换只能改全局配置项 `collect_port` 并重启。扩展侧唯一的端口入口
   是 popup 的输入框，只影响扩展自己请求哪个端口。
2. 打开浏览器的扩展管理页（`chrome://extensions` 或 `edge://extensions`）。
3. 打开右上角「开发者模式」。
4. 点「加载已解压的扩展程序」，选择本仓库的 `extension/` 目录。

## 安装（Firefox）

manifest 是 Chrome/Firefox 双兼容 MV3：`background` 同时带 `scripts`（Firefox 走
event page）和 `service_worker`（Chromium 走 SW）。Firefox 128+ 可用。

1. 启动 Trove 桌面应用（采集服务默认开启）。
2. 临时加载（重启浏览器后失效，日常调试用）：
   - 地址栏打开 `about:debugging#/runtime/this-firefox`
   - 点「临时载入附加组件…」，选择 `extension/manifest.json`（Firefox 选文件，不是目录）
3. **授予站点权限**（建议，Firefox MV3 默认不给 host 权限）：
   - `about:addons` → Trove 采集器 → 「权限」标签
   - 勾选「访问所有网站的数据」，跨域读图最稳
   - 采集服务本身已带 CORS 响应头（2026-09 起），不开权限通常也能连通
4. 永久安装（二选一）：
   - **AMO 自签名（推荐，免费且不公开）**：打包 `cd extension && zip -r ../trove-collector.xpi .`，
     到 [addons.mozilla.org](https://addons.mozilla.org/developers/) 提交为
     unlisted add-on，自动签名后下载 .xpi，在 `about:addons` ▸ 齿轮 ▸
     「从文件安装附加组件」装回。正式版 Firefox 只认签名包。
   - **Developer Edition / Nightly**：`about:config` 把
     `xpinstall.signatures.required` 设为 `false`，即可直接从文件安装未签名 .xpi。

> 注：Firefox 桌面支持系统通知（走 libnotify/桌面通知守护进程）；保存成功与否
> 一律以通知为准。

## 使用

- 右键网页上的图片 / 视频 → **保存到 Trove**，子菜单就是这个库的合集树；右键链接 →
  **保存链接文件到 Trove**（保存链接指向的文件）。三个上下文各挂一份同样的树，
  「最近」四格在最上面，分隔线以下是逐级展开的一级合集。
- **直接把图片/视频拖出页面**，光标旁会开同一个合集树：悬停 450ms 进下一级，松手在
  哪一格就存进哪一格；松手在面板外或按 Esc 是取消。拖拽只观察 `dragstart`，站
  点自己的拖拽行为不受影响；这个菜单可以在选项页关掉。
- **批量抓取本页媒体**：右键页面空白处（或某个 iframe 内）→「抓取本页媒体到
  Trove…」，或点扩展图标弹窗里的同名按钮。页面右侧开一个勾选面板
  （`picker.js`）：列出当前文档里的图片/视频，按面积从大到小，全选/清空，底部下拉
  选合集（或存默认位置），逐个保存——每个文件走的链路和右键保存完全一样（浏览器
  中继 → 服务端 `/fetch`），面板里看进度、每行标 ✓/✗。菜单项和弹窗按钮都是开关，
  面板开着再点一次即关闭；Esc 也能关。列出的范围：最长边小于 100px 的图标、4KB
  以下的内联占位图、`blob:` 源（视频站的 MediaSource 流读不了）不算；没加载过的
  懒加载图照列（尺寸标「尺寸未知」，缩略图点开才知道）。只扫当前文档：媒体在别的
  iframe 里就去那个 iframe 里右键；CSS 背景图、链接指向的文件不在列，后者用右键
  「保存链接文件」单存。
- **来源与授权标签（仅供参考）**：识别出国内外主流站点时，批量抓取面板顶部显示
  「来源：站点 · 授权标签」横幅，单张保存的系统通知末尾也会带上（如
  「已保存：xx.jpg（Unsplash · 可商用）」）。页面本身不认识时，逐行用图片 URL 的
  CDN 主机反推（如论坛盗链 `sinaimg.cn` → 微博）。识别逻辑在 `site-license.js`：
  站点许可表 → 页面内 CC 授权链接解析 → CDN 主机反推，三层取最强。标签只有参考
  意义，不是法律结论——详见下面的「来源与授权识别」一节。
- 子菜单里的「默认位置（不指定合集）」就是原来的行为：入库但不归类。
- 点击扩展图标可修改端口、测试连接、看当前是哪个库；右键图标 → **选项** 是三个开关
  （通知 / 保存后把 Trove 带到前台 / 拖拽菜单），存在 `chrome.storage.sync`。
- 图标变灰 + 徽标「未连」= 每 30 秒一次的探测没通；这时菜单里只会看到「（未连
  接）」一项。
- 保存前会先探测采集服务：Trove 未运行时立即提示，不会白下载。
- 抓图走「浏览器下载 → Trove 服务端下载」的回退链：浏览器请求带页面的登录
  会话，服务端请求带浏览器 UA 和 Referer —— 防盗链站点按表给源站 Referer
  （`hotlink-sites.js` 的 `SITES`：微博 / 知乎 / 哔哩哔哩 / 搜狐），不在表里就用
  来源页。Chromium 上浏览器那一侧的 Referer 由 `declarativeNetRequest` 动态规则
  改写（SW 里 `fetch` 设不了这个头）；Firefox 的规则API不支持改头时，这些站自动
  走服务端那条路，结果一样。
  64MB 是**扩展侧**的中继上限（`background.js` 的 `RELAY_MAX`，只看报文里的
  `content-length`），超过就直接走服务端：`/fetch` 流式落盘，不占浏览器内存。
  服务端另有 512MB 硬顶（`collect.rs` 的 `MAX_BODY`）—— `POST /add` 超出返回
  413，`/fetch` 下载超出按失败处理（502）。
- `data:` 图片直接解码保存；`blob:` 图片（≤32MB）从页面内读取。
- 「保存链接」指向网页（HTML）时会明确提示，不会把网页源码存进库。
- 指定的合集在执行时可能已经被删掉：那种情况文件照常入库、只是不进任何合集，并留
  一条日志 —— 一次采集不该因为目标没了而丢掉。反过来，`collection` 不是合法 id
  的请求会在服务端直接判 `400`：**静默落到别处比保存失败更糟**。

文件先落到 Trove 的收件目录：磁盘上是 `incoming/`（`paths::incoming_dir()`，
默认 `~/.local/share/trove/incoming`，可被 `TROVE_DATA_DIR` 挪走），几秒内自动导入
（见状态栏/通知）。

## 来源与授权识别（仅供参考，不是法律结论）

扩展会尽力回答「这张图来自哪、能不能商用」，但**没有任何自动化手段能真正判定一
张图的商用权利**——肖像权、商标、站方条款的最新版本都在浏览器视野之外。这里给
出的标签是三层启发式信号中最强的那个：

1. **站点许可表**（`site-license.js` 的 `SITES`，按 host 后缀匹配，含 CDN 域名）：
   - `可商用`：站方条款许可免费商用的图库——Unsplash / Pexels / Pixabay /
     PxHere / StockSnap / Burst；
   - `公共领域`：NASA、大都会博物馆、史密森尼、荷兰国立博物馆等开放馆藏；
   - `需购买授权`：Getty / Shutterstock / iStock / Adobe Stock / Dreamstime /
     Alamy / 123RF / 视觉中国 / 全景网；
   - `不可商用`：用户上传社区，版权归作者、平台不授商用——微博 / 知乎 / 哔哩
     哔哩 / 小红书 / 抖音 / TikTok / 微信公众号 / 花瓣 / LOFTER / 图虫 / 站酷 /
     豆瓣 / 快手 / 今日头条 / Instagram / Facebook / X / Pinterest / Reddit /
     Tumblr / YouTube / Vimeo / Dribbble / Behance / ArtStation；
   - `暂无法判断`：搜索聚合页与门户（Google / Bing / 百度 / 搜狗 / 360 / 新浪 /
     搜狐 / 网易 / 腾讯）——图片来自第三方，授权要看原站。
2. **页面 CC 授权链接**：Flickr / Wikimedia / Openverse / Europeana 这类逐张授
   权的站点，解析页面里的 `creativecommons.org` 链接——CC0 → `可商用`，BY →
   `CC·需署名`（可商用），ND → `CC·禁改编`（可商用），含 NC → `CC·禁商用`；
   页面上找不到链接就标 `逐张确认`。
3. **CDN 主机反推**：页面本身不认识时，用图片 URL 的主机匹配同一张表（ CDN 域
   名都在表里，如 `sinaimg.cn`、`zhimg.com`、`hdslb.com`、`staticflickr.com`）。

匹配优先级：页面站点 > 图片主机；`不可商用` 的判断只代表「平台/站方不授权商
用」，不代表作者不会授权。识别结果是参考标签，商用前请自行确认授权；站点条款
变化以站方页面为准。

## 连不上？

「测试连接」失败时按顺序排查：

1. Trove 桌面应用是否在运行（采集服务跑在 Trove 进程里，关掉应用服务就停）。
2. 端口是否一致：扩展侧改 popup 的输入框（存 `chrome.storage.local`，只决定扩展请求
   哪个端口）；Trove 侧监听端口来自全局配置项 `collect_port`，默认 23916，改完要重启。
   设置窗口里没有采集服务页面，端口被占时也不会自动换端口。
3. **服务端 CORS 需要 2026-09 之后的 Trove 版本**——旧版服务无 CORS 头且不回
   OPTIONS 预检，浏览器扩展一定连不上；重新编译并重启 Trove。
4. Firefox 确认已勾选「访问所有网站的数据」权限（见上）。
5. 连得上却没有合集（子菜单写着「读取合集列表失败」）：`GET /collections` 是 2026-10
   加的端点，旧版 Trove 会回 404；这时保存仍然可用，只是都落进默认位置。重建并重启
   Trove 即恢复。Trove 没打开任何库时它回 `503`，同样是「有保存、没有目标列表」。
