# Trove Collector（浏览器扩展）

把网页上的图片一键存进本地 Trove 素材库。通过 Trove 的本地采集服务
（`http://127.0.0.1:23916`）上传，Trove 运行中即会自动导入，来源 URL
会写入资产的 source 字段。

## 安装（Chrome / Edge / Firefox MV3）

1. 启动 Trove 桌面应用（采集服务默认开启，端口见 设置 ▸ 通用 ▸ 采集服务）。
2. 打开浏览器的扩展管理页（`chrome://extensions` 或 `edge://extensions`）。
3. 打开右上角「开发者模式」。
4. 点「加载已解压的扩展程序」，选择本仓库的 `extension/` 目录。

## 使用

- 右键网页上的图片 → **Save image to Trove**。
- 右键链接 → **Save linked file to Trove**（保存链接指向的文件）。
- 点击扩展图标可修改端口并测试与 Trove 的连接。

文件先落到 Trove 的 inbox 目录，几秒内自动导入（见状态栏/通知）。
