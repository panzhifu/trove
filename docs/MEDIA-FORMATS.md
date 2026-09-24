# 媒体格式支持 (Media Formats)

> 图片、视频、音频、3D、字体、动效 — 全格式支持一览

---

## 格式总览

| 类别 | 格式数 | 支持格式 |
|------|--------|---------|
| 图片 | 12 | PNG, JPG, WebP, AVIF, TIFF, SVG, PSD, HEIC, JXL, BMP, ICO, RAW |
| 视频 | 6 | MP4, MOV, MKV, WebM, AVI, FLV |
| 音频 | 12 | mp3, wav, flac, m4a, aac, ogg, oga, opus, wma, aiff, aif, aifc —— 可管理，**也可播放**（见下文「音频格式」） |
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
| OpenEXR | exr crate（经 image） | ✅ | ✅ | 场景线性浮点，按内置曲线显示，见下「高动态范围」 |
| Radiance HDR | image crate | ✅ | ✅ | `.hdr`，同样是浮点线性 |
| TGA | image crate | ✅ | ✅ | 无 magic bytes：像素按后缀解，**尺寸**要指名格式才读得到 |

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

### 高动态范围（OpenEXR / Radiance HDR）

浮点像素不是"更清晰的图"，而是**线性场景辐射值**，常常远超 `0..=1`。直接交给 8 位编码器等于把它当已归一化的值截断——渲染出来的 EXR 就是一块近黑的方板，上面有几个过曝的光斑。所以解出来之后必须先过一条显示曲线（`media/hdr.rs`）：曝光增益 `2^stops` → ACES 胶片拟合（Narkowicz 那条近似）→ sRGB 传输函数 → 量化到 8 位。Alpha 是覆盖率不是光，不参与这条链。

| 项 | 现状 |
|------|------|
| 解码 | `image` 的 `exr`/`hdr` 特性（`exr` crate 本就在依赖图里，`hdr` 在 image 内零依赖） |
| 显示 | 固定 0 档曝光、按 scene-linear 解释；**没有** OCIO/ACES 工作室配置，也**没有**逐资产输入空间覆盖 |
| 曝光滑杆 | 无。Serpent 每次改档就重跑一遍 oiiotool；Trove 这边要么常驻一个 4K 浮点缓冲（约 500 MB），要么也起进程，两头都不划算，所以先不做 |
| 多 part / plane | 不选，只解第一个 |
| 像素编辑 | **就地只读**：8 位重编码会把 HDR 数据换成一张它的降级副本，工具条直接说明原因而不是给一颗会失败的按钮 |

### 图片功能

| 功能 | 说明 |
|------|------|
| EXIF 读取 | 相机参数、GPS、拍摄时间 |
| 色彩分析 | 主色板提取 |
| 直方图 | 亮度/色彩分布统计 |
| 感知哈希 | dHash（9×8 差异哈希）用于以图搜图 |
| 颜色直方图 | 用于颜色相似度搜索 |
| 像素编辑 | 旋转/翻转/裁剪；没有就地编码器的那些格式（EXR/HDR/TGA/RAW/HEIC/PSD/SVG/JXL）在预览工具条上就说明原因 |
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
| 倍速播放 | 9 档 0.25× – 4×，音调保持 |
| 时间线跳转 | 精确到帧 |
| 截当前帧入库 | 播放头那一帧按源尺寸存成 PNG 资产（一次 ffmpeg，落 `incoming/` 后链接导入） |
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

> **本节只列代码里真有的能力。** 音频现在可导入、去重、打标签、排序、筛选、搜索，**也能播放**：
> `components/preview/audio.rs` 复用视频那套音频引擎（ffmpeg 出 44.1 kHz 立体声 PCM + rodio），
> 播控是 `components/preview/transport.rs` 与视频共享的同一份控件，上方还画一条**波形包络**。
> 没有频谱。

### 支持的格式

归类只有一张表：`media::probe::is_audio_ext`。「打开方式」的音频档以前自己抄了一份列表，
两份漂过一次（`flv`/`ts` 在视频那边漂过），现在它委托给同一张表。

| 扩展名 | 归类 | mime | 时长 | 标签 | 缩略图（封面 / 波形） | 播放 | 倍速/音量 | 波形 | 频谱 |
|------|------|------|------|------|------|------|------|------|------|
| mp3 | ✅ | `audio/mpeg` | ✅ | ✅ ID3 | ⚠️ 见下 | ✅ | ✅ | ✅ | ❌ |
| wav | ✅ | `audio/wav` | ✅ | ❌ | ⚠️ 见下 | ✅ | ✅ | ✅ | ❌ |
| flac | ✅ | `audio/flac` | ✅ | ✅ Vorbis comment | ⚠️ 见下 | ✅ | ✅ | ✅ | ❌ |
| m4a | ✅ | `audio/mp4` | ✅ | ✅ MP4 atom | ⚠️ 见下 | ✅ | ✅ | ✅ | ❌ |
| aac | ✅ | `audio/aac` | ✅ | — | ⚠️ 见下 | ✅ | ✅ | ✅ | ❌ |
| ogg | ✅ | `audio/ogg` | ✅ **实测** | ✅ Vorbis comment | ⚠️ 见下 | ✅ | ✅ | ✅ | ❌ |
| oga | ✅ | `audio/ogg` | ✅ **实测** | ✅ | ⚠️ 见下 | ✅ | ✅ | ✅ | ❌ |
| opus | ✅ | `audio/opus` | ✅ | ✅ | ⚠️ 见下 | ✅ | ✅ | ✅ | ❌ |
| wma | ✅ | `audio/x-ms-wma` | ✅ | ✅ ASF | ⚠️ 见下 | ✅ | ✅ | ✅ | ❌ |
| aiff / aif | ✅ | `audio/aiff` | ⚠️ 未实测 | — | ⚠️ 见下 | ✅ | ✅ | ✅ | ❌ |
| aifc | ✅ | `audio/x-aifc` | ⚠️ 未实测 | — | ⚠️ 见下 | ✅ | ✅ | ✅ | ❌ |

「缩略图」一列对所有音频格式都是同一个意思，分两步：`thumb::ensure`（导入路径）用 lofty 读
**文件内嵌的封面**，有就当卡片；没有封面时**只用已经缓存的波形包络**画一张卡片，缓存里没有就
回落到 kind 图标 —— 导入热路径**不**为一张卡片起 ffmpeg。补上这张卡片的钱由维护付：
Settings ▸ 文件 ▸「重新生成缩略图」（`thumb::regenerate` 的音频分支会先 `load_or_build` 再画，
见 [BACKUP-MAINTENANCE.md](./BACKUP-MAINTENANCE.md)）；或者你先把这首歌播一次，包络就进了缓存。
能力与扩展名无关，所以不给逐行的 ✅/❌（那会凭空造出我没测过的差别）。
只在 `mp3` + ID3v2.3 `APIC`(PNG) 上实测过：卡片是封面本身，且 **32×32 文件图标（APIC 类型 1/2）会被跳过**，
否则"取第一张图"会选中图标、糊掉整张卡。有封面时封面优先，包络只兜底。
**标「实测」的两行是真的跑过**：拿 `/usr/share/sounds/freedesktop/stereo/*.oga` 走
`import::stage_all` → `commit_staged` → `assets::query`，库里落的是
`kind=Audio mime=audio/ogg duration=Some(6128)` 与 `duration=Some(1313)`。其余行是"代码路径通、
按 lofty 对该容器的支持推断"，没有逐个文件验证过，**别当成测量结果**。
AIFF 两行标未实测：归类与 mime 由单元测试钉住，但时长只造过一个手工 AIFF 去试，
我自己的 80 位采样率编码是错的（lofty 读出 `sr=4294967295`、`duration=0`），
所以既没证明能读、也没证明不能读。值得一提：`duration=0` 时 `mine_audio` 的
`!duration.is_zero()` 守卫生效，写进库的是 `None` 而不是一个假时长。

标签与时长都出自 lofty 0.21（`media/metadata.rs` 的 `mine_audio`），不依赖 ffmpeg，
所以 ffmpeg 缺失时依然有值。`mine_audio` 走 `Probe::open(..).guess_file_type()`：
lofty 认 `.ogg` 但**不认 `.oga` 这个扩展名**（同一份字节改名就报
"No format could be determined"），改为先看内容、认不出再回落到扩展名提示，`.oga` 就能读了；
对认得出的文件这一步是 no-op（`guess` 结果 `or` 上原类型）。

`wma` 之前被归类为音频却没有 mime 条目，一路落到 `application/octet-stream`，已补。

### 数据实际落在哪

| 取到的东西 | 去处 | 可见 / 可搜 |
|------|------|------|
| `title` 标签 | 写成资产标题（`MinedMetadata::title`） | ✅ 检查器可见、全文索引可搜 |
| `artist` / `album` | `facts.media` | ✅ 检查器属性页显示（有才显示）；❌ **仍不可搜**——索引只喂 file_name / title / description / tags，接进索引是 P2 且要升 `INDEX_VERSION` |
| `duration` | `assets.duration_ms` 列 | ✅ 可排序、可筛选，列表与 CLI 都读得到 |
| 采样率 / 声道 / 位深 / 比特率 | `facts.audio`（`sample_rate()` / `channels()` / `bit_depth()` / `audio_bitrate()`，**kbps 不是 bps**） | ✅ 检查器合成一行 `48 kHz · 2 ch · 160 kbps`，缺哪项就不显示哪项 |

值得注意：时长是 `MineStage` 从 lofty 拿到的，`ProbeStage` 没有音频分支（`pipeline.rs` 的 match 里音频走
`_ => {}`），所以音频**不会**为探测额外起一个 ffprobe 进程。

### 音频功能（现状）

| 功能 | 说明 |
|------|------|
| 重新挖掘 | Settings ▸ 文件 ▸「重新挖掘元数据」：给库里已有资产重读内嵌标签与流属性，让新版本加的字段能回填而不必重新导入。**只覆盖 `facts.audio` / `facts.media`（字体则 `facts.font`）**，视觉签名、色板、AI 标签与 `unknown` 键原样保留 —— 整包覆盖会抹掉检索指纹和 AI 结果 |
| 播放 | 预览面板的传输控件：播放/暂停、时间线拖动（松手才 seek）、`m:ss / m:ss`、九档倍速（0.25×–4×，`atempo` 链保音调）、音量弹层 |
| 波形包络 | 400 桶、0=静音。请求 ffmpeg 出**单声道 300 Hz**（swresample 先低通再抽取，抽取后的样本本身就是包络），三分钟文件是 ~5.4 万样本而非 ~800 万，所以**首次播放时现算**也不卡；结果缓存在 `<cache>/libraries/<slug>/waveforms/<sha[:2]>/<sha>.bin`，与缩略图一样按内容寻址、可删可重建。不在导入管线里 —— 见 [IMPORT-PIPELINE.md](./IMPORT-PIPELINE.md) |
| 不自动播放 | 刻意的：网格方向键每按一下换一个文件，自动播放会把浏览变成点唱机 |
| 无封面时的卡片 | 没有内嵌封面的音频，卡片就是它的**波形包络**（512×288，与字体样张卡、模型卡同一种"画一张卡"的做法，用同一份 `waveform::bitmap`）。导入时只在包络**已经**缓存的情况下画，缺的那张由「重建缩略图」补 —— 见 [BACKUP-MAINTENANCE.md](./BACKUP-MAINTENANCE.md) |
| 去重 | 内容哈希（BLAKE3）在导入时生效，与资产类型无关 |
| 标签管理 | 与其他资产一致：批量标签、评分、收藏、描述 |
| 打开方式 | 发现系统里的外部播放器 |
| 完整性校验 | 逐文件重算 BLAKE3，音频同样覆盖 |

> 这一节曾经写着 9 种格式全部支持播放、波形图、频谱分析，并列出"rodio 内嵌播放器""导入时生成波形"。
> 写下那句话时这些都不存在。播放与波形现在确实有了（上表），**频谱仍然没有**，波形也不是导入时算的；
> rodio 一直在依赖里，先服务视频配乐、现在也服务这个面板。

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
| EPUB | ❌ 只有「打开方式」里有它，`probe` 不认，所以归 `Other` | ✅ |

> 文档类**没有真缩略图**：`thumb::ensure` 对 `Document` / `Archive` / `Other` 里非文本的那些仍返回 `None`，卡片是 kind 图标。PDF 首页光栅这一项已经比过路线（外部 `pdftoppm`/`mutool`/`gs` 链 vs 随包 pdfium），2026-09-24 的决定是**宁可这行留在差距表里也不加依赖**，见 [GAP-TO-SERPENT.md](./GAP-TO-SERPENT.md) 的 §内容类型计划。

## 文本格式

### 支持的格式

一份清单，两个用途（查看器与卡片）都读它：`trove-core/src/media/text.rs::is_text_ext`。

| 家族 | 后缀 |
|------|------|
| 纯文本与标记 | `txt` `text` `log` `md` `markdown` `mdx` `rst` `tex` `adoc` |
| 数据与配置 | `json` `jsonc` `json5` `csv` `tsv` `xml` `html` `htm` `yaml` `yml` `toml` `ini` `cfg` `conf` `properties` `plist` `xmp` `env` |
| 样式、脚本、源码 | `css` `scss` `sass` `less` `js` `mjs` `cjs` `ts` `tsx` `jsx` `vue` `svelte` `php` `py` `rb` `pl` `pm` `lua` `r` `jl` `dart` `go` `zig` `rs` `java` `kt` `kts` `swift` `scala` `c` `h` `cc` `cpp` `cxx` `hpp` `hh` `cs` |
| 着色器与查询 | `glsl` `hlsl` `wgsl` `vert` `frag` `sql` `proto` `graphql` |
| 脚本与构建 | `sh` `bash` `zsh` `fish` `bat` `cmd` `ps1` `cmake` `mk` `make` `diff` `patch` `po` `pot` |
| 定时文本与联系人 | `srt` `vtt` `ass` `sub` `ics` `vcf` |
| 整名即后缀的那些 | `editorconfig` `gitignore` `gitattributes` `dockerignore`（没有点分后缀，导入时整个文件名会落到 `ext` 上） |

`.svg` 不在其中（那是图片，Trove 会真的渲染它），`.pdf` / `.doc` 也不在（读不了内容，别用一屏乱码冒充预览）。

### 文本功能

| 功能 | 说明 |
|------|------|
| 只读查看器 | gpui-kit 的 `Editor` 元素：行号槽、虚拟行、选择与复制，折行可切换（本次会话内） |
| 编码探测 | BOM → UTF-16 的 NUL 奇偶 → 二进制判定 → 严格 UTF-8 → `chardetng` 统计猜测；头部会报这个文件被当成什么编码 |
| 载入上限 | 1 MiB，**截断会说出来**；查看器只读，所以截断的缓冲不可能被存回源文件 |
| 文本卡片 | 前 11 行、每行 72 列，走 SVG 光栅那条路（系统字体带 fallback） |
| 就地编辑 | 无。写回是另一件事，需要 CAS（Trove 没有 `revisions` 表，比较对象只能是 mtime + size），本轮没做 |

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
