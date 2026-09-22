# 配置管理 (Configuration)

> 应用配置、库配置、快捷键、多语言 — 个性化设置全解

---

## 概述

Trove 的配置分为两个层级：

| 层级 | 文件 | 作用域 |
|------|------|--------|
| 应用配置 | `config.json` | 全局（所有库共享） |
| 库配置 | `<library>/library.json` | 单个素材库 |

```
~/.config/trove/
├── config.json          # 应用配置
├── keybindings.json     # 快捷键
└── themes/              # 主题

~/Documents/Trove/
└── <slug>/
    ├── library.db       # 数据库
    ├── library.json     # 库配置
    ├── data/            # Blob 存储
    ├── thumbs/          # 缩略图缓存
    ├── backups/         # 自动备份
    └── search_index/    # 搜索索引
```

---

## 应用配置 (config.json)

### 结构

```json
{
  "libraries": [
    { "slug": "default", "name": "My Library", "path": "~/Documents/Trove/default" }
  ],
  "active_library": "default",
  "language": "zh-CN",
  "keybindings": { "workspace/open": "enter" },
  "grid_zoom": 1.0,
  "undo_cap": 100,
  "filter_tools": ["type", "rating", "tags", "color"],
  "collect_enabled": true,
  "collect_port": 23916,
  "point_enhance": true,
  "appearance": "System",
  "theme_light": null,
  "theme_dark": null,
  "min_preview_zoom": 0.1,
  "max_preview_zoom": 10.0,
  "disabled_plugins": [],
  "plugin_settings": {
    "sidecar-notes": { "mode": "override" }
  },
  "embedding": {
    "base_url": "https://api.openai.com/v1",
    "api_key": "",
    "model": "text-embedding-3-small"
  },
  "watch_folders": ["/path/to/watch"]
}
```

### 配置项详解

#### 界面

| 键 | 类型 | 默认值 | 说明 |
|----|------|--------|------|
| `language` | `string?` | `null`（跟随系统） | 界面语言 |
| `appearance` | `enum` | `System` | `Light` / `Dark` / `System` |
| `theme_light` | `string?` | `null` | 浅色主题名 |
| `theme_dark` | `string?` | `null` | 深色主题名 |
| `grid_zoom` | `float?` | `1.0` | 网格缩放倍率 |
| `filter_tools` | `string[]?` | 默认集 | 工具栏筛选工具 |

#### 行为

| 键 | 类型 | 默认值 | 说明 |
|----|------|--------|------|
| `undo_cap` | `int?` | `100` | 撤销历史深度 (1-500) |
| `min_preview_zoom` | `float?` | `0.1` | 最小预览缩放 |
| `max_preview_zoom` | `float?` | `10.0` | 最大预览缩放 |
| `point_enhance` | `bool?` | `true` | 点云增强（EDL） |

#### 采集服务

| 键 | 类型 | 默认值 | 说明 |
|----|------|--------|------|
| `collect_enabled` | `bool?` | `true` | 是否启用本地采集 |
| `collect_port` | `int?` | `23916` | 采集服务端口 |

#### AI 嵌入

| 键 | 类型 | 默认值 | 说明 |
|----|------|--------|------|
| `embedding.base_url` | `string` | `""` | 嵌入 API 地址 |
| `embedding.api_key` | `string` | `""` | API Key |
| `embedding.model` | `string` | `""` | 模型名称 |

#### 插件

| 键 | 类型 | 默认值 | 说明 |
|----|------|--------|------|
| `disabled_plugins` | `string[]` | `[]` | 禁用插件列表 |
| `plugin_settings` | `object` | `{}` | 插件专属设置 |

---

## 库配置 (library.json)

### 结构

```json
{
  "watch_folders": ["/path/to/folder1", "/path/to/folder2"]
}
```

### 配置项

| 键 | 类型 | 说明 |
|----|------|------|
| `watch_folders` | `string[]` | 监视文件夹列表 |

被监视文件夹跳过哪些内容不是配置项：文件夹自己的 `.gitignore` / `.ignore` /
`.git/info/exclude` 说了算，规则同 Git —— 见 [TASKS-BACKGROUND.md](TASKS-BACKGROUND.md) 的「忽略规则」。

---

## 快捷键

### 默认快捷键

| 操作 | 快捷键 | 说明 |
|------|--------|------|
| 打开资产 | `Enter` | 预览选中资产 |
| 全选 | `Ctrl+A` | 选中所有资产 |
| 删除 | `Delete` | 移入回收站 |
| 永久删除 | `Shift+Delete` | 跳过回收站 |
| 收藏 | `F` | 切换收藏状态 |
| 评分 1-5 | `1-5` | 设置评分 |
| 标签面板 | `T` | 打开标签面板 |
| 搜索 | `Ctrl+F` | 聚焦搜索框 |
| 设置 | `Ctrl+,` | 打开设置 |
| 撤销 | `Ctrl+Z` | 撤销操作 |
| 重做 | `Ctrl+Shift+Z` | 重做操作 |
| 复制 | `Ctrl+C` | 复制到剪贴板 |
| 粘贴导入 | `Ctrl+Shift+V` | 剪贴板图片入库 |
| 刷新 | `F5` | 刷新视图 |
| 全屏 | `F11` | 切换全屏 |
| 放大 | `Ctrl+=` | 放大预览 |
| 缩小 | `Ctrl+-` | 缩小预览 |
| 实际大小 | `Ctrl+0` | 重置缩放 |
| 密度+ | `Ctrl+]` | 增大网格密度 |
| 密度- | `Ctrl+[` | 减小网格密度 |

### 自定义快捷键

在"设置 → 快捷键"中修改：

```json
{
  "workspace/open": "enter",
  "workspace/select-all": "ctrl-a",
  "workspace/delete": "delete",
  "asset/favorite": "f",
  "asset/rate-1": "1",
  "search/focus": "ctrl-f"
}
```

### 快捷键格式

| 格式 | 示例 | 说明 |
|------|------|------|
| 单键 | `enter`, `delete`, `f` | 无修饰键 |
| 组合键 | `ctrl-a`, `ctrl-shift-z` | 多修饰键 |
| 特殊键 | `space`, `tab`, `escape` | 功能键 |
| 媒体键 | `media-play-pause` | 多媒体键 |

---

## 多语言

### 支持的语言

| 代码 | 语言 | 状态 |
|------|------|------|
| `en` | English | ✅ |
| `zh-CN` | 简体中文 | ✅ |
| `ja` | 日本語 | ✅ |
| `ko` | 한국어 | ✅ |
| `es` | Español | ✅ |
| `fr` | Français | ✅ |
| `de` | Deutsch | ✅ |
| `pt` | Português | ✅ |
| `ru` | Русский | ✅ |

### 语言切换

- 设置 → 外观 → 语言
- 实时切换，无需重启
- `null` 或 `System` 跟随操作系统

### 翻译文件结构

```
trove-app/src/i18n/
├── en.toml
├── zh-CN.toml
├── ja.toml
└── ...
```

```toml
# zh-CN.toml
app_title = "Trove"
welcome = "欢迎使用 Trove"
library = "素材库"
settings = "设置"
```

---

## 外观设置

### 主题

| 主题 | 说明 |
|------|------|
| System | 跟随系统深浅色 |
| Light | 强制浅色 |
| Dark | 强制深色 |

### 密度

| 密度 | 说明 |
|------|------|
| 紧凑 | 最小间距，最多内容 |
| 默认 | 平衡间距 |
| 舒适 | 较大间距 |

### 网格缩放

- 滑杆范围：0.5× – 2.0×
- 影响缩略图大小和行高
- 工具栏滑杆 + 快捷键 `Ctrl+]` / `Ctrl+[`

---

## 文件设置

### 导入方式

| 方式 | 说明 |
|------|------|
| 链接（默认） | 不复制文件，记录路径 |
| 复制 | 复制到库目录（特殊场景） |

### 监视文件夹

- 设置中添加/移除目录
- 新增文件自动入库
- 不归入集合

### 缩略图

| 设置 | 说明 |
|------|------|
| 缩略图质量 | JPEG 质量 (1-100) |
| 最大尺寸 | 长边像素 (256-1024) |
| 缓存清理 | 手动清理缩略图缓存 |

---

## 搜索设置

### 全文搜索

| 设置 | 说明 |
|------|------|
| 索引重建 | 手动重建搜索索引 |
| 中文分词 | 启用 jieba 分词 |
| 拼音搜索 | 启用拼音匹配 |
| 模糊匹配 | 启用容错搜索 |

### AI 嵌入

| 设置 | 说明 |
|------|------|
| 端点 URL | OpenAI 兼容 API 地址 |
| API Key | 认证密钥 |
| 模型名称 | 嵌入模型名 |
| 测试连接 | 验证配置 |
| 手动回填 | 触发嵌入计算 |

---

## 设置页结构

```
设置
├── 通用
│   ├── 语言
│   ├── 外观（深浅色）
│   └── 自动更新检查
├── 文件
│   ├── 导入方式
│   ├── 监视文件夹
│   └── 缩略图设置
├── 搜索
│   ├── 索引重建
│   └── AI 嵌入配置
├── 快捷键
│   └── 所有可绑定操作
├── 插件
│   └── 插件列表 + 设置页
└── 关于
    ├── 版本信息
    ├── 检查更新
    └── 开源许可
```

---

## 配置迁移

### 版本升级

- 启动时自动迁移
- 新增配置项使用默认值
- 废弃配置项静默忽略

### 手动迁移

1. 导出配置：`config.json` + `keybindings.json`
2. 在新机器上放置到配置目录
3. 启动 Trove

---

## 代码位置

| 文件 | 内容 |
|------|------|
| `trove-core/src/config.rs` | 配置结构体、加载/保存 |
| `trove-core/src/paths.rs` | 路径计算 |
| `trove-core/src/keybindings.rs` | 快捷键解析 |
| `trove-app/src/app/i18n.rs` | 多语言支持 |
| `trove-app/src/app/theme.rs` | 主题管理 |
| `trove-app/src/app/actions.rs` | 操作定义 |
| `trove-app/src/dialogs/settings/mod.rs` | 设置对话框 |
| `trove-app/src/dialogs/settings/appearance.rs` | 外观设置 |
| `trove-app/src/dialogs/settings/files.rs` | 文件设置 |
| `trove-app/src/dialogs/settings/search.rs` | 搜索设置 |
| `trove-app/src/dialogs/settings/shortcuts.rs` | 快捷键设置 |
| `trove-app/src/dialogs/settings/plugins.rs` | 插件设置 |
| `trove-app/src/dialogs/settings/ai.rs` | AI 设置 |
| `trove-app/src/dialogs/settings/about.rs` | 关于页 |
