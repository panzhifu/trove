# 媒体格式支持 (Media Formats)

> 图片、视频、音频、3D、字体、动效 — 全格式支持一览

---

## 格式总览

| 类别 | 格式数 | 支持格式 |
|------|--------|---------|
| 图片 | 12 | PNG, JPG, WebP, AVIF, TIFF, SVG, PSD, HEIC, JXL, BMP, ICO, RAW |
| 视频 | 6 | MP4, MOV, MKV, WebM, AVI, FLV |
| 音频 | 9 | MP3, WAV, FLAC, AAC, OGG, M4A, WMA, AIFF, Opus |
| 3D 模型 | 6 | OBJ, STL, PLY, glTF, GLB, .blend |
| 字体 | 5 | TTF, OTF, WOFF, WOFF2, TTC |
| 动图 | 3 | GIF, 动态 WebP, APNG |
| 文档 | 9 | PDF, DOC, DOCX, XLS, XLSX, PPT, PPTX, TXT, MD |
| 压缩包 | 6 | ZIP, RAR, 7Z, TAR, GZ, BZ2 |

---

## 图片格式

### 栅格格式

| 格式 | 解码库 | 缩略图 | 预览 | 备注 |
|------|--------|--------|------|------|
| PNG | image crate | ✅ | ✅ | 含透明度 |
| JPEG | image crate | ✅ | ✅ | 标准解码 |
| WebP | image crate | ✅ | ✅ | 含动画 |
| AVIF | image crate | ✅ | ✅ | 需系统解码器 |
| TIFF | image crate | ✅ | ✅ | 多页支持 |
| BMP | image crate | ✅ | ✅ | 标准解码 |
| ICO | image crate | ✅ | ✅ | Windows 图标 |
| JXL | jxl-oxide | ✅ | ✅ | JPEG-XL 解码 |

### 矢量/专业格式

| 格式 | 解码库 | 缩略图 | 预览 | 备注 |
|------|--------|--------|------|------|
| SVG | resvg/usvg | ✅ | ✅ | 矢量渲染 |
| PSD | psd crate | ✅ | ✅ | 合成预览 |
| HEIC | 系统 heif-dec | ✅ | ✅ | 原生解码 |
| RAW | rawler | ✅ | ✅ | 去马赛克→白平衡→sRGB |

### 相机 RAW 支持

| 品牌 | 格式 | 支持 |
|------|------|------|
| Canon | CR2, CR3 | ✅ |
| Nikon | NEF, NRW | ✅ |
| Sony | ARW, SR2 | ✅ |
| Fujifilm | RAF | ✅ |
| Panasonic | RW2 | ✅ |
| Olympus | ORF | ✅ |
| Adobe | DNG | ✅ |

### 图片功能

| 功能 | 说明 |
|------|------|
| EXIF 读取 | 相机参数、GPS、拍摄时间 |
| 色彩分析 | 主色板提取 |
| 直方图 | 亮度/色彩分布统计 |
| 感知哈希 | pHash 用于以图搜图 |
| 颜色直方图 | 用于颜色相似度搜索 |
| 像素编辑 | 旋转/翻转/裁剪 |
| 格式转换 | JPEG/PNG/WebP/BMP/TIFF |

---

## 视频格式

### 支持的容器

| 容器 | 探测方式 | 解码 | 预览 |
|------|---------|------|------|
| MP4 | mp4 crate (moov box) | ffmpeg | ✅ |
| MOV | mp4 crate (moov box) | ffmpeg | ✅ |
| MKV | ffprobe | ffmpeg | ✅ |
| WebM | ffprobe | ffmpeg | ✅ |
| AVI | ffprobe | ffmpeg | ✅ |
| FLV | ffprobe | ffmpeg | ✅ |
| MPEG-TS | ffprobe | ffmpeg | ✅ |

### 视频功能

| 功能 | 说明 |
|------|------|
| 有声预览 | ffmpeg 解码 + rodio 播放 |
| 音画同步 | 音频时钟同步 |
| 倍速播放 | 0.5× – 2×，音调保持 |
| 时间线跳转 | 精确到帧 |
| 截图采集 | 当前帧入库为 PNG |
| 海报帧 | 自动生成缩略图 |

### 视频管线

```
┌─────────────────────────────────────────────────────────┐
│                    ffmpeg 子进程                         │
│                                                         │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  │
│  │ 视频解码管道  │  │ 音频解码管道  │  │ ffprobe 探测 │  │
│  │ (raw video)  │  │ (PCM 44.1kHz)│  │ (元数据)     │  │
│  └──────────────┘  └──────────────┘  └──────────────┘  │
└─────────────────────────────────────────────────────────┘
```

---

## 音频格式

### 支持的格式

| 格式 | 探测 | 播放 | 波形图 | 频谱 |
|------|------|------|--------|------|
| MP3 | ✅ | ✅ | ✅ | ✅ |
| WAV | ✅ | ✅ | ✅ | ✅ |
| FLAC | ✅ | ✅ | ✅ | ✅ |
| AAC | ✅ | ✅ | ✅ | ✅ |
| OGG | ✅ | ✅ | ✅ | ✅ |
| M4A | ✅ | ✅ | ✅ | ✅ |
| WMA | ✅ | ✅ | ✅ | ✅ |
| AIFF | ✅ | ✅ | ✅ | ✅ |
| Opus | ✅ | ✅ | ✅ | ✅ |

### 音频功能

| 功能 | 说明 |
|------|------|
| 波形图 | 导入时生成 |
| 频谱分析 | 频率分布可视化 |
| 播放预览 | rodio 内嵌播放器 |
| 音量控制 | 滑块调节 |
| 标签管理 | 批量标签编辑 |

---

## 3D 模型格式

### 支持的格式

| 格式 | 解析器 | 导入 | 预览 | 备注 |
|------|--------|------|------|------|
| OBJ | 自研 | ✅ | ✅ | 标准网格 |
| STL | 自研 | ✅ | ✅ | 3D 打印格式 |
| PLY | 自研 | ✅ | ✅ | 点云 + 网格 |
| glTF | gltf crate | ✅ | ✅ | JSON 描述 |
| GLB | gltf crate | ✅ | ✅ | 二进制容器 |
| .blend | Blender 无头 | ✅ | ✅ | 转 GLB 后预览 |

### 3D 功能

| 功能 | 说明 |
|------|------|
| GPU 渲染 | wgpu 驱动 |
| 交互 | 旋转/缩放/平移 |
| 着色 | Lambert + Blinn-Phong |
| 眼罩光照 | EDL 效果 |
| 背面剔除 | 闭合网格 |
| 大文件 | 流式常驻 + 抽稀采样 |
| Meshlet | GPU 逐簇视锥剔除 |
| 空间索引 | 可选 `.trovecloud` |

### 3D 视口架构

```
┌─────────────────────────────────────────┐
│              wgpu 渲染                  │
│  ┌───────────────────────────────────┐  │
│  │  Lambert + Blinn-Phong 着色       │  │
│  │  眼罩光照 (EDL)                   │  │
│  │  背面剔除 + 补洞                  │  │
│  └───────────────────────────────────┘  │
│                                         │
│  降级: CPU 软件光栅器 (无 GPU 时)        │
└─────────────────────────────────────────┘
```

---

## 字体格式

### 支持的格式

| 格式 | 解析 | 缩略图 | 预览 | 安装 |
|------|------|--------|------|------|
| TTF | ✅ | ✅ | ✅ | ✅ |
| OTF | ✅ | ✅ | ✅ | ✅ |
| WOFF | ✅ | ✅ | ✅ | ❌ |
| WOFF2 | ✅ | ✅ | ✅ | ❌ |
| TTC | ✅ | ✅ | ✅ | ❌ |

### 字体功能

| 功能 | 说明 |
|------|------|
| 样张卡片 | 导入时用字体自身渲染 |
| 占位符 | `{name}` / `{family}` 可自定义 |
| 实况预览 | 主区域完整样张 |
| 安装/卸载 | 一键安装到系统 |
| 字体信息 | 族名、样式、字重 |

---

## 动图格式

### 支持的格式

| 格式 | 播放 | 帧控制 | 缩略图 |
|------|------|--------|--------|
| GIF | ✅ | ✅ | 静态首帧 |
| 动态 WebP | ✅ | ✅ | 静态首帧 |
| APNG | ✅ | ✅ | 静态首帧 |

### 动图功能

| 功能 | 说明 |
|------|------|
| 逐帧播放 | 预览和检查器中播放 |
| 帧控制 | 暂停/逐帧/速度 |
| 静态缩略图 | 网格中保持静态 |

---

## 设计格式

### 支持的格式

| 格式 | 缩略图 | 预览 | 说明 |
|------|--------|------|------|
| PSD | ✅ | ✅ | psd crate 合成 |
| AI | ❌ | ❌ | 仅识别 |
| Sketch | ❌ | ❌ | 仅识别 (Open With) |
| Figma | ❌ | ❌ | 仅识别 (Open With) |
| XD | ❌ | ❌ | 仅识别 (Open With) |

---

## 文档格式

### 支持的格式

| 格式 | 识别 | Open With |
|------|------|-----------|
| PDF | ✅ | ✅ |
| DOC/DOCX | ✅ | ✅ |
| XLS/XLSX | ✅ | ✅ |
| PPT/PPTX | ✅ | ✅ |
| TXT | ✅ | ✅ |
| MD | ✅ | ✅ |
| RTF | ✅ | ✅ |
| EPUB | ✅ | ✅ |

---

## 压缩包格式

### 支持的格式

| 格式 | 识别 | Open With |
|------|------|-----------|
| ZIP | ✅ | ✅ |
| RAR | ✅ | ✅ |
| 7Z | ✅ | ✅ |
| TAR | ✅ | ✅ |
| GZ | ✅ | ✅ |
| BZ2 | ✅ | ✅ |

---

## 探测优先级

文件类型探测按以下顺序：

1. **扩展名匹配** — 快速路径
2. **文件头魔数** — 准确判断
3. **内容解析** — 最终确认

---

## 代码位置

| 文件 | 内容 |
|------|------|
| `trove-core/src/media/probe.rs` | 文件类型探测 |
| `trove-core/src/media/thumb.rs` | 缩略图生成 |
| `trove-core/src/media/formats/mod.rs` | 3D 格式入口 |
| `trove-core/src/media/formats/obj.rs` | OBJ 解析 |
| `trove-core/src/media/formats/stl.rs` | STL 解析 |
| `trove-core/src/media/formats/ply.rs` | PLY 解析 |
| `trove-core/src/media/formats/gltf.rs` | glTF/GLB 解析 |
| `trove-core/src/media/formats/blend.rs` | Blender 无头导出 |
| `trove-core/src/media/formats/meshlet.rs` | Meshlet 聚类 |
| `trove-core/src/media/formats/streaming/` | 大文件流式读取 |
| `trove-core/src/media/video.rs` | ffmpeg 视频管线 |
| `trove-core/src/media/convert.rs` | 格式转换 |
| `trove-core/src/media/edit.rs` | 像素编辑 |
| `trove-app/src/components/preview/` | 预览组件 |
