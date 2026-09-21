# 浏览器扩展 (Browser Extension)

> Chrome MV3 扩展 + 本地采集服务 — 网页素材一键入库

---

## 概述

Trove 提供两种从浏览器采集素材的方式：

| 方式 | 说明 |
|------|------|
| Chrome MV3 扩展 | 右键菜单发送图片到本地 Trove |
| 本地采集服务 | HTTP API (`127.0.0.1:23916`)，支持脚本/扩展直接调用 |

---

## Chrome MV3 扩展

### 架构

```
┌─────────────────────────────────────────────┐
│               Chrome 浏览器                  │
│  ┌───────────────────────────────────────┐  │
│  │         popup.html / popup.js          │  │
│  │  - 显示连接状态                        │  │
│  │  - 发送当前图片                        │  │
│  └───────────────────┬───────────────────┘  │
│                      │                       │
│  ┌───────────────────▼───────────────────┐  │
│  │         background.js (Service Worker) │  │
│  │  - 右键菜单注册                        │  │
│  │  - 图片数据获取                        │  │
│  │  - 发送到本地 Trove                    │  │
│  └───────────────────┬───────────────────┘  │
└──────────────────────┼──────────────────────┘
                       │ HTTP POST
                       ▼
            ┌─────────────────────┐
            │  Trove 本地采集服务   │
            │  127.0.0.1:23916    │
            └─────────────────────┘
```

### 扩展文件结构

```
extension/
├── manifest.json    # MV3 扩展清单
├── background.js    # Service Worker (后台脚本)
├── popup.html       # 弹出窗口
├── popup.js         # 弹出窗口脚本
├── icon.png         # 扩展图标
└── icons/           # 多尺寸图标
```

### 安装方式

1. 打开 Chrome → `chrome://extensions/`
2. 启用"开发者模式"
3. 点击"加载已解压的扩展程序"
4. 选择 `extension/` 目录

### manifest.json

```json
{
  "manifest_version": 3,
  "name": "Trove Collector",
  "version": "1.0",
  "description": "Send images from web pages to your local Trove library",
  "permissions": [
    "contextMenus",
    "activeTab",
    "scripting"
  ],
  "host_permissions": [
    "http://127.0.0.1:23916/*",
    "<all_urls>"
  ],
  "background": {
    "service_worker": "background.js"
  },
  "action": {
    "default_popup": "popup.html"
  }
}
```

### 右键菜单

- 右键图片 → "发送到 Trove"
- 右键页面 → "采集页面所有图片"
- 支持 PNG、JPEG、WebP、AVIF、GIF

### 工作流程

1. 用户右键点击网页图片
2. background.js 获取图片 URL
3. 直接发送图片数据到 `http://127.0.0.1:23916/add`
4. Trove 接收后自动入库

---

## 本地采集服务

### 服务配置

| 配置项 | 默认值 | 说明 |
|--------|--------|------|
| 启用 | `true` | 是否启动采集服务 |
| 端口 | `23916` | HTTP 服务端口 |
| 地址 | `127.0.0.1` | 仅本地回环 |

### API 端点

#### POST /add — 直接添加图片

```http
POST http://127.0.0.1:23916/add
Content-Type: image/png

<raw image bytes>
```

- 直接接收原始图片字节
- 自动启动导入管线
- 返回资产 ID

#### POST /fetch — URL 抓取

```http
POST http://127.0.0.1:23916/fetch
Content-Type: application/json

{
  "url": "https://example.com/image.png"
}
```

- 服务端下载 URL 指向的图片
- 自动入库
- 记录来源 URL

### 响应格式

```json
{
  "status": "ok",
  "asset_id": "uuid",
  "file_name": "image.png"
}
```

错误响应：

```json
{
  "status": "error",
  "message": "service disabled"
}
```

### 使用场景

| 场景 | 调用方式 |
|------|---------|
| 浏览器扩展 | `fetch('http://127.0.0.1:23916/add', {method:'POST', body: blob})` |
| curl 脚本 | `curl -X POST --data-binary @image.png http://127.0.0.1:23916/add` |
| Python 脚本 | `requests.post('http://127.0.0.1:23916/fetch', json={'url': img_url})` |

### 安全考虑

- 仅监听 `127.0.0.1` — 不接受外部连接
- 无认证 — 假设本地环境可信
- 可选禁用 — 设置中关闭采集服务

---

## 状态检测

### 扩展侧

popup.js 定期检测服务状态：

```js
async function checkConnection() {
  try {
    const res = await fetch('http://127.0.0.1:23916/', { method: 'HEAD' });
    return res.ok;
  } catch {
    return false;
  }
}
```

- 绿色指示灯：服务在线
- 红色指示灯：服务离线（提示启动 Trove）

### Trove 侧

- 设置中显示服务状态
- 可手动启停
- 端口冲突时自动尝试下一个端口

---

## 代码位置

| 文件 | 内容 |
|------|------|
| `extension/manifest.json` | Chrome MV3 扩展清单 |
| `extension/background.js` | Service Worker 后台脚本 |
| `extension/popup.html` | 弹出窗口 HTML |
| `extension/popup.js` | 弹出窗口脚本 |
| `trove-core/src/services/collect.rs` | 本地采集服务实现 |
| `trove-app/src/app/library_manager.rs` | 服务启动与管理 |
