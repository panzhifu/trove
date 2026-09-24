# 导入管线 (Import Pipeline)

> 素材从文件到入库的完整处理流程 — 一次解码，多处复用

---

## 概述

导入管线是 Trove 的核心数据入口。每个文件从原始路径到最终成为可搜索的资产，经过一系列**阶段 (Stage)** 处理。管线采用**声明式依赖图**设计：每个阶段声明自己需要什么（`needs`）、产生什么（`produces`），运行时自动拓扑排序并插入缺失的默认阶段。

**核心设计原则**：
- **只链接不复制** — 文件留在原位，Trove 只记录路径与内容哈希
- **一次解码多处复用** — 图片只解码一次，缩略图/色板/视觉签名共享同一缓冲区
- **阶段容错** — 单个阶段失败不会中止整个文件，只有致命错误（如无法哈希）才跳过

---

## 管线流程图

```
源文件
  │
  ▼
┌─────────────┐  产出: blake3, size, rel_path
│  HashStage  │  成本: IO
│   哈希阶段   │  读取源文件一次，计算 BLAKE3
└──────┬──────┘
       │
       ▼
┌─────────────┐  产出: kind, mime, width, height, duration
│ ProbeStage  │  成本: Cheap
│   探测阶段   │  从文件头读取类型/尺寸/时长（无需解码像素）
└──────┬──────┘
       │
       ▼
┌─────────────┐  产出: Decoded (small + rgb + dims)
│ DecodeStage │  成本: CPU
│   解码阶段   │  解码图片一次，降采样至 512px，供后续阶段共享
└──────┬──────┘
       │
       ▼
┌─────────────┐  产出: thumb path
│ ThumbStage  │  成本: IO
│  缩略图阶段  │  写入缩略图缓存 (thumbs/<sha[:2]>/<sha>.jpg)
└──────┬──────┘
       │
       ▼
┌─────────────┐  产出: MinedMetadata
│  MineStage  │  成本: IO
│  元数据阶段  │  EXIF + 色板 + 时长 → 写入 mined 结构
└──────┬──────┘
       │
       ▼
┌─────────────┐  产出: dHash + color histogram
│ VisualSig   │  成本: CPU
│ 视觉签名阶段 │  感知哈希 + 颜色直方图 → 用于以图搜图
└──────┬──────┘
       │
       ▼
   数据库入库
```

---

## 阶段详解

### HashStage — 哈希阶段

| 属性 | 值 |
|------|-----|
| 产出 | `Need::Hash` |
| 成本 | `Cost::Io` |
| 输入 | 源文件路径 |

- 计算源文件的 BLAKE3 哈希
- 对于复制导入（`ImportStorage::Copy`），将文件暂存到 blob 目录
- 对于链接导入（`ImportStorage::Link`），仅记录路径

### ProbeStage — 探测阶段

| 属性 | 值 |
|------|-----|
| 依赖 | `Need::Hash` |
| 产出 | `Need::Probe` |
| 成本 | `Cost::Cheap` |

- 从文件头读取 MIME 类型和资产类型（图片/视频/音频/字体/3D/文档）
- 图片：读取尺寸（部分格式无需完整解码）
- 视频：MP4 从 moov box 读取；其他格式调用 ffprobe
- 音频：读取采样率/声道数/时长

### DecodeStage — 解码阶段

| 属性 | 值 |
|------|-----|
| 依赖 | `Need::Probe` |
| 产出 | `Need::Decode` |
| 成本 | `Cost::Cpu` |

- **仅对图片执行**
- 解码后降采样至 `THUMB_MAX` (512px)
- 生成 `Decoded` 结构：
  - `small`: 降采样后的缩略图
  - `rgb`: RGB 像素缓冲区（供色板和签名使用）
  - `dims`: 原始尺寸
- **重导入优化**：如果缩略图已缓存，直接解码缓存而非原图

### ThumbStage — 缩略图阶段

| 属性 | 值 |
|------|-----|
| 依赖 | `Need::Probe` |
| 可选读取 | `Need::Decode` |
| 产出 | `Need::Thumb` |
| 成本 | `Cost::Io` |

- 写入缩略图到缓存目录
- 路径格式：`thumbs/<sha前2位>/<sha>.jpg`
- 不同资产类型的缩略图策略：
  - 图片：从解码缓冲区降采样
  - 视频：ffmpeg 提取海报帧
  - 字体：渲染样张卡片
  - 3D：渲染模型卡片

### MineStage — 元数据阶段

| 属性 | 值 |
|------|-----|
| 可选读取 | `Need::Decode` |
| 成本 | `Cost::Io` |

- 提取 EXIF 元数据（相机参数、GPS、拍摄时间）
- 从 RGB 缓冲区计算主色板
- 合并时长信息

### VisualSigStage — 视觉签名阶段

| 属性 | 值 |
|------|-----|
| 依赖 | `Need::Decode` |
| 成本 | `Cost::Cpu` |

- 计算差异哈希 (dHash) — 用于以图搜图
- 计算颜色直方图 — 用于颜色相似度搜索
- 仅对图片执行

---

## 每种类型实际经过哪些阶段

阶段列表对所有人一致，**分支靠在阶段内自查 `io.kind`**（而不是维护多条管线）。下表是读代码时最想先知道
的那件事：某个类型的文件到底做了什么、什么被跳过。✅ = 有实际工作，— = 立即返回 `Ok`。

| 阶段 | 图片 | 视频 | **音频** | 字体 | 3D | 文档 / 压缩包 / 其他 |
|------|------|------|---------|------|----|--------------------|
| Hash | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Probe | ✅ kind·mime·头部尺寸 | ✅ kind·mime·moov / ffprobe 尺寸时长 | ✅ kind·mime | ✅ kind·mime | ✅ kind·mime | ✅ kind·mime |
| Decode | ✅ 唯一一次解码 | — | **—** | — | — | — |
| Thumb | ✅ 降采样 | ✅ ffmpeg 海报 | ⚠️ **仅当内嵌封面存在，或波形包络已在缓存**（否则 `None` → kind 图标） | ✅ 样张卡片 | ✅ 正交卡片 | — |
| Mine | ✅ EXIF + 主色板 | —（时长由 Probe 带入） | ✅ **lofty：标题 / 艺术家 / 专辑 / 时长** | ✅ 族·样式·字重 | — | — |
| VisualSig | ✅ | — | — | — | — | — |

音频这一行值得单独说明三点：

1. **它在管线里，且没有空转。** `DecodeStage` / `VisualSigStage` 各自开头就是 `if io.kind != AssetKind::Image { return Ok(()) }`，
   `ThumbStage` 对音频只在**文件里真有内嵌封面**、或**波形包络已经被缓存**时产出缩略图，否则回到 `None` 让 kind 图标顶上：
   跳过是显式契约，不是漏配，而且这条路径上音频**不会**为一张卡片起 ffmpeg。整条管线音频只付一次文件读 + 一次 lofty 解析。
2. **`ProbeStage` 没有音频分支**，所以音频时长不是探出来的而是 `MineStage` 挖出来的，随后由它自己
   把 `io.duration_ms` 与 `io.mined.duration_ms` 对齐（`pipeline.rs` 的 MineStage::run）。好处是音频永远不会为
   探时长去抢一个 ffmpeg 进程槽。
3. **波形不在管线里**，是首次播放时现算并单独缓存（`media/waveform.rs`）。刻意不挂进导入：它要读完整段音频，
   而导入的正确性不该取决于一个纯展示用的派生物；而且管线失败会跳过整个文件，一个算不出包络的音频
   不该因此进不了库。同一份包络后来还兼作无封面音频的**网格卡片**，付费的地方是
   [BACKUP-MAINTENANCE.md](./BACKUP-MAINTENANCE.md) 里的缩略图重建，不是这里。
4. **导入即完整**：时长写进 `assets.duration_ms` 列、标题写进资产 `title`、艺术家/专辑写进 `facts.media`。
   这意味着后续补播放器或补缩略图**都不需要重新导入**——缩略图按内容哈希寻址且可懒生成，
   检查器要读的字段早就在行里了。

---

## 两阶段提交

管线分为两个阶段，分离慢速文件操作和快速数据库操作：

```
后台线程                    主线程
─────────                  ─────────
stage_source()  ──────►  commit_staged()
  │ 管线执行                 │ 去重检查
  │ Hash → Probe → Decode   │ 插入 asset 行
  │ → Thumb → Mine → Sig    │ 写入标签/集合
  │                         │ 更新搜索队列
```

**`stage_source`**（后台）：
- 执行完整的管线阶段
- 返回 `StagedFile` 结构（包含所有中间产物）

**`commit_staged`**（主线程）：
- 去重：检查内容哈希（BLAKE3）是否已存在
- 插入资产记录
- 写入标签/集合关联
- 入队搜索索引更新

---

## 去重策略

导入时自动去重：

1. **内容去重**：BLAKE3 相同 → 复用已有资产记录
2. **视觉去重**：dHash 汉明距离 ≤ 阈值 → 标记为重复图片
3. **重复处理**：保留最新，其余入回收站

---

## 批量像素编辑

导入后支持批量编辑：

- 旋转（90°/180°/270°）
- 水平/垂直翻转
- 裁剪（百分比坐标）
- 链接文件：**写回原文件**
- 存储文件：替换 blob

---

## 批量格式转换

- 输出格式：JPEG / PNG / WebP / BMP / TIFF
- 可选长边限制
- 可选重新导入转换后文件

---

## 代码位置

| 文件 | 内容 |
|------|------|
| `trove-core/src/media/pipeline.rs` | 管线引擎、阶段 trait、默认管线 |
| `trove-core/src/media/import.rs` | 导入协调、两阶段提交 |
| `trove-core/src/media/thumb.rs` | 缩略图缓存管理 |
| `trove-core/src/media/metadata.rs` | 元数据提取 |
| `trove-core/src/media/color.rs` | 色板计算 |
| `trove-core/src/media/search.rs` | 视觉签名（dHash + 直方图） |
| `trove-core/src/media/probe.rs` | 文件类型探测 |
| `trove-core/src/media/blob.rs` | Blob 暂存与哈希 |
