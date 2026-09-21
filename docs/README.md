# Trove 文档索引

> **Trove** — 本地 · 私有 · 你自己的素材库  
> 基于 Rust + GPUI 构建的跨平台素材资产管理工具

---

## 文档目录

| 文档 | 内容 |
|------|------|
| [IMPORT-PIPELINE.md](./IMPORT-PIPELINE.md) | 素材导入管线 — 哈希、探测、解码、缩略图、元数据、视觉签名 |
| [SEARCH-INDEXING.md](./SEARCH-INDEXING.md) | 搜索与索引 — 全文搜索 (Tantivy)、视觉搜索、向量嵌入、智能集合 |
| [PREVIEW-SYSTEM.md](./PREVIEW-SYSTEM.md) | 预览系统 — 图片/视频/音频/3D/字体/Lottie 多格式预览 |
| [AI-EMBEDDING.md](./AI-EMBEDDING.md) | AI 与嵌入 — OpenAI 兼容嵌入、向量语义搜索、模型管理 |
| [TAG-COLLECTION.md](./TAG-COLLECTION.md) | 标签与集合 — 层级标签、合集、智能合集、批量操作 |
| [BROWSER-EXTENSION.md](./BROWSER-EXTENSION.md) | 浏览器扩展 — Chrome MV3 扩展、本地采集服务 |
| [TASKS-BACKGROUND.md](./TASKS-BACKGROUND.md) | 任务与后台作业 — 任务管理器、进度、取消、重试 |
| [BACKUP-MAINTENANCE.md](./BACKUP-MAINTENANCE.md) | 备份与维护 — 自动备份、回收站、完整性校验、XMP 导出 |
| [PLUGIN-SYSTEM.md](./PLUGIN-SYSTEM.md) | 插件系统 — 插件管线、命令、设置页、参考实现 |
| [MEDIA-FORMATS.md](./MEDIA-FORMATS.md) | 媒体格式支持 — 图片/视频/音频/3D/字体/动效格式一览 |
| [CONFIGURATION.md](./CONFIGURATION.md) | 配置管理 — 应用配置、库配置、快捷键、多语言 |
| [INTERACTION-SYSTEM.md](./INTERACTION-SYSTEM.md) | 交互系统 — 三视图布局、工作区、检查器、对话框 |

---

## 核心架构

```
┌─────────────────────────────────────────────────────────────┐
│                        trove-app (GPUI)                      │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌─────────────┐ │
│  │ 工作台    │  │ 检查器   │  │ 资源管理器│  │  对话框     │ │
│  │ Workspace │  │Inspector │  │ Explorer │  │  Dialogs    │ │
│  └──────────┘  └──────────┘  └──────────┘  └─────────────┘ │
├─────────────────────────────────────────────────────────────┤
│                        trove-core                           │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌─────────────┐ │
│  │ 导入管线  │  │ 搜索引擎  │  │ AI/嵌入  │  │  媒体处理   │ │
│  │ Pipeline  │  │ Search   │  │   AI     │  │   Media     │ │
│  └──────────┘  └──────────┘  └──────────┘  └─────────────┘ │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌─────────────┐ │
│  │ 存储层    │  │ 任务管理  │  │ 插件系统  │  │  服务层     │ │
│  │  Store   │  │  Tasks   │  │ Plugins  │  │  Services   │ │
│  └──────────┘  └──────────┘  └──────────┘  └─────────────┘ │
├─────────────────────────────────────────────────────────────┤
│                     SQLite (rusqlite)                        │
└─────────────────────────────────────────────────────────────┘
```

## 技术栈

| 层级 | 技术 |
|------|------|
| UI 框架 | GPUI (Zed 编辑器的 Rust UI 框架) |
| GPU 渲染 | wgpu (3D 视口) |
| 数据库 | SQLite (rusqlite) |
| 全文搜索 | Tantivy |
| 图像处理 | image crate / resvg / psd / rawler / jxl-oxide |
| 视频处理 | ffmpeg (子进程) |
| 3D 解析 | 自研 OBJ/STL/PLY/glTF 解析器 |
| 音频播放 | rodio |
| 嵌入推理 | OpenAI 兼容 API (ureq) |
| 序列化 | serde / serde_json |
| i18n | rust-i18n |
