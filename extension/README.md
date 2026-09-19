# Trove 采集器（浏览器扩展）

把网页上的图片一键存进本地 Trove 素材库。通过 Trove 的本地采集服务
（`http://127.0.0.1:23916`）上传，Trove 运行中即会自动导入，来源 URL
会写入资产的 source 字段。

## 安装（Chrome / Edge）

1. 启动 Trove 桌面应用（采集服务默认开启，端口见 设置 ▸ 通用 ▸ 采集服务）。
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

- 右键网页上的图片 → **保存图片到 Trove**。
- 右键链接 → **保存链接文件到 Trove**（保存链接指向的文件）。
- 点击扩展图标可修改端口并测试与 Trove 的连接。
- 保存前会先探测采集服务：Trove 未运行时立即提示，不会白下载。
- 抓图走「浏览器下载 → Trove 服务端下载」的回退链：浏览器请求带页面的登录
  会话，服务端请求带浏览器 UA 和来源页 Referer，防盗链图片大概率也能拿下；
  超过 64MB 的大文件直接走服务端（落盘流式，不占浏览器内存）。
- `data:` 图片直接解码保存；`blob:` 图片（≤32MB）从页面内读取。
- 「保存链接」指向网页（HTML）时会明确提示，不会把网页源码存进库。

文件先落到 Trove 的 inbox 目录，几秒内自动导入（见状态栏/通知）。

## 连不上？

「测试连接」失败时按顺序排查：

1. Trove 桌面应用是否在运行（采集服务跑在 Trove 进程里，关掉应用服务就停）。
2. 端口是否一致（扩展 popup 与 设置 ▸ 通用 ▸ 采集服务）。
3. **服务端 CORS 需要 2026-09 之后的 Trove 版本**——旧版服务无 CORS 头且不回
   OPTIONS 预检，浏览器扩展一定连不上；重新编译并重启 Trove。
4. Firefox 确认已勾选「访问所有网站的数据」权限（见上）。
