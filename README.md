<h1 align="center">Trove</h1>

<p align="center"><b>本地 · 私有 · 你自己的素材库</b></p>

<p align="center">
  <a href="./README.en.md">English</a> ·
  <a href="./LICENSE">MIT</a>
</p>

<br>

<p align="center">
  <img src="docs/screenshots/main-window.png" alt="Trove 主窗口" width="900"/>
</p>

<p align="center"><i>对齐缩略图网格 · 停靠布局 · 3D 模型视口 · 视觉搜索 —— 全都跑在本地，数据永不上传</i></p>

---

## 快速开始

```sh
git clone https://github.com/panzhifu/trove.git && cd trove
cargo run -p trove-app
```

> 需要 [Rust 工具链](https://www.rust-lang.org/tools/install)。首次启动打开**欢迎界面**：左边列出已有的素材库，右边给它起个名字就能开始。素材库由 Trove 按系统约定存放，不必（也不能）自己挑目录。

[Trove 文档索引](docs/README.md)

---

## 功能特性

### 组织与检索

| | |
|---|---|
| **全文搜索** | 基于 Tantivy，覆盖文件名 / 标题 / 描述 / 标签名，支持相关性排序、词语 / 子串 / 拼音三种匹配（`sunse` 命中 `Sunset`，`mao` 命中 花园里的猫），可叠加任意筛选条件 |
| **查询表达式** | 搜索框读得懂结构：`猫 狗` 与、`猫 \| 狗` 或、`-猫` 排除、`"夏日 海边"` 当作一个短语；限定符 `name:` `title:` `desc:` `tag:` 只搜某一处，`ext:` `kind:` `path:` `rating:` `fav:` 直接当筛选条件（各带别名，`-` 取反）。只写限定符即是筛选而非搜索，看不懂的片段照旧检索并在状态栏说明 |
| **视觉搜索** | 以图搜图 + 按颜色搜索 —— 64 位差异哈希 (dHash) + 4096 桶颜色直方图，导入时自动计算，零推理依赖 |
| **智能集合** | 规则驱动的虚拟文件夹，JSON 查询树，支持评分 / 类型 / 文本 / 标签 / 收藏 / 颜色 / 拍摄日期 / 宽高比 / 方向，`and` / `or` 组合，编译期校验 |
| **标签** | 层级嵌套、大小写不敏感、带色标；筛选 / 计数 / 智能集合自动包含子树 |
| **评分 & 收藏** | 1–5 星评分，一键收藏 |
| **集合树** | 嵌套文件夹、多对多归属，拖拽改父级，循环检测 |

### 导入与收集

| | |
|---|---|
| **拖拽导入** | 文件管理器拖进窗口即可，整窗都是拖放面 |
| **粘贴导入** | `Ctrl+Shift+V`，剪贴板图片直接入库 |
| **从 URL 导入** | 后台下载并自动入库，记录来源 URL |
| **监视文件夹** | 设置中添加目录，新增文件自动入库（不归入集合） |
| **忽略规则** | Git 风格：读取文件夹自己的 `.gitignore` / `.ignore` / `.git/info/exclude`，决定监视与拖拽导入时跳过哪些内容；被忽略的目录不再进入 |
| **导入方式** | 只链接，不复制：文件留在你原来的位置，Trove 只记录路径与内容哈希。源文件移动或删除后可 BLAKE3 校验重新链接 |
| **RAW / HEIC / SVG / PSD** | 相机 RAW 走 rawler 管线（去马赛克 → 白平衡 → sRGB），HEIC 经系统 heif-dec 转换；SVG 矢量渲染，PSD 合成内嵌预览 |
| **设计格式缩略图** | 挖掘 EXIF / 音频标签（标题·艺术家·专辑·时长） / 字体族·样式·字重 / MP4 尺寸时长；有 ffmpeg 时自动生成视频海报；音频文件内嵌封面时直接当卡片用，没有封面的用它的**波形包络**画一张卡（导入时不额外解码，由「重建缩略图」补） |

### 浏览与查看

| | |
|---|---|
| **三视图** | 网格（对齐布局）/ 列表 / 时间线，带密度缩放滑杆、多选 + 浮动工具栏 |
| **形状与比例筛选** | 工具栏形状筛选（横 / 竖 / 方）内置媒体人常用比例预设 —— 公众号封面 2.35:1、视频封面 16:9、短视频竖屏 9:16、照片 4:3 / 3:4、方形 1:1，3% 容差匹配圆整尺寸 |
| **检查器** | 缩略图 + 标签 + 色板 + 行内编辑（标题 / 描述 / 来源链接 / 评分），属性页显示 MIME / 大小 / 尺寸 / 内容哈希（BLAKE3），一键定位源文件 |
| **动图播放** | GIF / 动态 WebP / APNG 在预览与检查器中逐帧播放；网格缩略图保持静态以保证性能 |
| **字体实况预览** | 导入时用字体自身渲染「样张卡片」缩略图，样卡带家族名副标题；样张按拉丁 / 中日韩 / 数字三行取字形，字体缺哪行就自动补默认行；主区可放大为实时样张并平移缩放；检查器可把字体安装 / 卸载到系统 |
| **最近查看** | 左栏侧边最近 200 个资产，入回收站自动隐藏、恢复后回归 |

### 3D 模型预览

| | |
|---|---|
| **格式** | OBJ / STL / PLY / glTF / GLB 作为一等资产类型导入，带格式专属图标；`.blend` 经 Blender 无头转 GLB 后预览 |
| **GPU 视口** | wgpu 驱动，旋转 / 缩放 / 平移，双面 Lambert + Blinn-Phong 着色；无 GPU 时自动回退到 CPU 软件光栅器 |
| **观感** | 眼罩光照 (EDL) + 补洞 + 闭合网格背面剔除 |
| **大文件** | 流式常驻预算 + 抽稀，20 GB 不 OOM；覆盖度保持采样，缩放 / 移动全顺滑；离线空间索引可选（`.trovecloud`，9 B/点，60% 体积），目前由 `index_build` 示例离线生成 |
| **网格剔除** | 大网格 (≥8192 面) 做 meshlet 聚类 + GPU 逐簇视锥剔除 |

### 视频与截图

| | |
|---|---|
| **音频与有声预览** | 音频文件在预览面板里直接播放（封面 + 波形包络 + 传输控件）；视频则是 ffmpeg 逐帧解码 + 音频管线（rodio 播放 44.1 kHz 立体声 PCM），播放 / 暂停 / 跳转 / 时间线 / 音量 / 静音 / 倍速（0.25×–4× 九档，`atempo` 链保持音调），音画同步走音频时钟 |
| **截图采集** | 菜单里一个「截图」入口：打开选取器后拖拽框选，或在提供窗口列表的会话上点选窗口，结果直接入库 PNG。进程内优先（KWin D-Bus → xcap），无后端时回落外部工具（grim+slurp / scrot / macOS screencapture） |
| **批量像素编辑** | 旋转 / 翻转 / 裁剪（百分比坐标，按每张图自身尺寸解析），JPEG 质量参数，原地替换媒体文件并保留资产身份与组织关系；链接文件的编辑结果**写回原文件**（预览快捷编辑会先确认，批量对话框有醒目提示）；预览界面标题栏带单图快捷编辑（顺/逆时针旋转、水平/垂直翻转、打开编辑对话框），结果即刻回显到画面 |
| **批量格式转换** | 图片重新编码为 JPEG / PNG / WebP / BMP / TIFF，可选长边限制，可重新导入转换后文件 |
| **XMP 元数据导出** | 在媒体文件旁写入标准 XMP sidecar（标题 / 描述 / 标签 / 评分），原子写入、全转义、不破坏原文件 |

### 维护与安全

| | |
|---|---|
| **回收站** | 删除 → 回收站 → 恢复 / 永久删除；清空回收站释放 blob 与缩略图 |
| **孤儿清理** | 移除不再被引用的 blob 文件 |
| **完整性校验** | 重算每个存储文件的 BLAKE3 并与记录比对，问题资产一键入回收站 |
| **自动备份** | SQLite `VACUUM INTO` 快照到 `backups/`（每天至多一次，滚动保留 10 份） |
| **整包备份导出** | 素材库管理器或 File ▸ 「导出备份压缩包…」：软件配置（偏好 / 快捷键 / 插件设置 / 库注册表）+ 全部素材库数据（数据库快照 / library.json / media blob）+ incoming 收件箱，打包为一个 zip（manifest.json 记录恢复布局）；缓存目录属可再生数据，不进包 |
| **重复文件查找** | 按差异哈希聚类视觉相同的图片，每组「保留最新、其余入回收站」（签名晚于资产导入，老资产需先在设置里回填） |
| **存储占用统计** | 按目录列出 Trove 自己写进磁盘的东西（设置与主题 / 数据库 / 备份 / 缩略图与索引 / 日志 / 收件箱），标出哪些删掉只是花时间重建 |
| **多素材库** | 新建 / 切换 / 删除具名素材库；每个库有自己的数据库、监视文件夹与缩略图缓存，统计面板实时展示各类型数量 / 总容量 |

### 扩展

| | |
|---|---|
| **本地采集服务** | `http://127.0.0.1:23916`，`POST /add`（原始字节）与 `POST /fetch`（服务端抓取），浏览器扩展直连 |
| **浏览器扩展** | `extension/` 内置 MV3 扩展，右键发送网页图片到运行中的 Trove |
| **多语言界面** | 9 种语言实时切换：English、简体中文、日本語、한국어、Español、Français、Deutsch、Português、Русский；默认跟随系统 |

---

## 界面布局

| 停靠位置 | 面板 | 用途 |
|---|---|---|
| 顶 | 标题栏 | File / Settings 按钮、窗口控制 |
| 左 | 资源管理器 | 收藏夹树、智能集合、最近查看、回收站 |
| 中 | 工作区 | 对齐缩略图网格 + 搜索 |
| 右 | 标签 + 检查器 | 标签筛选与资产详情 |

- **File** 菜单导入文件；**Settings** 打开设置窗口，七个内置页 + 插件页：**关于**（版本 / 检查更新 / 语言）· **外观**（深浅模式 / 主题 / 自定义主题）· **文件**（素材库 / 存储占用 / 缩略图 / 元数据重挖掘 / 备份 / 清理与校验）· **模型**（点云观感 / 预览缩放）· **搜索**（全文索引 / 视觉指纹）· **AI**（端点与供应商 / 向量库 / 自动打标）· **快捷键**，其后追加插件的设置页（当前是 sidecar notes）
- 还没有任何素材库时，启动进入**欢迎界面**而不是主窗口——素材库是记录与配置的落脚点，没有它主界面无事可做
- 工作区网格采用对齐布局（Google 相册风格），任意宽度下撑满面板
- 工作区标题栏内嵌弹出式搜索框，旁显示当前视图资产数
- 整窗都是拖放面；`Ctrl/Cmd+点击` 或 `Shift` 范围选择
- **系统托盘**：关闭窗口仅最小化到托盘（KDE/freedesktop StatusNotifierItem / Windows 通知图标 / macOS NSStatusItem），托盘菜单可恢复窗口或彻底退出

---

## 项目结构

```
crates/
├── trove-core/          # 领域、持久化与服务层（无 UI）
│   ├── src/
│   │   ├── model/          # 纯数据类型 (Asset, Collection, Tag, …)
│   │   ├── store/          # SQLite 层：schema、CRUD、智能查询、统计
│   │   ├── media/          # 导入、探测、缩略图、编辑、渲染、3D/点云解析与索引
│   │   ├── services/       # 备份、维护、存储占用、采集服务、截图、XMP、字体、更新
│   │   ├── tasks/          # 后台任务：导入 job、文件夹监听
│   │   ├── history/        # 撤销/重做、最近颜色等应用历史
│   │   ├── library.rs      # 对 store + 数据根 / 缓存根的高级封装
│   │   ├── paths.rs        # 平台标准目录（config / data / cache / state）
│   │   ├── search.rs       # Tantivy 全文索引 + search_queue 发件箱 drain
│   │   ├── layout.rs       # 对齐网格布局（动态规划）
│   │   ├── config.rs       # 全局偏好 + 素材库注册表
│   │   └── keybindings.rs  error.rs
└── trove-app/           # gpui-kit 桌面 UI
    ├── src/
    │   ├── main.rs          # GPUI 引导、菜单、快捷键、欢迎 / 主窗口分流
    │   ├── app/             # 窗口壳层：根视图、欢迎界面、标题栏、动作、主题、i18n、托盘
    │   ├── library/         # LibraryController、导入任务、文件夹监听
    │   ├── dialogs/         # 设置各页、规则编辑器、重复文件查找、批量重命名、格式转换
    │   ├── panels/          # 资源管理器、文件夹、工作区、标签、检查器
    │   └── components/      # 预览组件（图片 / 视频 / 字体 / 3D / 回退）+ 视频配乐引擎
└── trove-cli/           # 无界面的 `trove` 命令行（脚本与 AI 使用）
    └── src/
        ├── cli.rs           # clap 命令树 —— `--help` 即完整契约
        ├── ctx.rs           # 库打开、输出契约、退出码
        ├── read.rs          # 只读命令：libraries / info / list / search / get / doctor …
        └── write.rs         # 写命令：import / set / tag / trash / collection / index
```

核心数据表：

```
assets             资产记录
collections        嵌套文件夹
asset_collection   资产-收藏夹多对多
tags               标签（不区分大小写，parent_id 成树）
asset_tag          资产-标签关联
smart_collections  智能集合（规则过滤）
search_queue       全文索引发件箱：触发器入队，drain 喂给 Tantivy
view_history       最近查看记录（上限 200 条）
model_looks        每个 3D 资产的着色观感（按字段着色 / 色带 / 轴）
asset_embeddings   向量嵌入，语义搜索的资产侧
ai_analysis        AI 分析结果缓存（带提示词版本指纹）
```

磁盘结构（Linux 路径；macOS / Windows 用各自的 `data` / `cache` / `config` 目录）：

```
~/.config/trove/                  设置、主题、历史
~/.local/share/trove/
├── libraries/<名称>/             每个素材库一个目录
│   ├── library.db                单文件数据库
│   ├── library.json              该库自己的偏好（监视的文件夹）
│   └── backups/                  每日数据库快照
└── incoming/                     截图与浏览器扩展收到的文件（被链接后留在原处）
~/.cache/trove/libraries/<名称>/  缩略图、全文索引 —— 可随时删除并重建
~/.local/state/trove/logs/        日志
```

素材文件本身**不在**这些目录里：导入只记录路径，文件留在你原来的位置。

---

## 命令行

`trove-cli` 是桌面版的无声兄弟：与 app 共用同一份 `trove-core`，读同一个数据库、遵守同一套规则，并且**可以在桌面版开着的时候运行**——此时素材库以只读方式打开，查询照常，写入只落数据库，索引的更新留给持有它的那个进程。

```sh
cargo build -p trove-cli            # 产出 target/debug/trove

trove libraries                     # 列出机器上的所有库
trove info                          # 当前库概览：数量、体积、索引状态
trove list --kind image -n 20       # 列素材（筛选参数见 --help）
trove search 猫 --aspect wechat-cover
trove get <uuid>                    # 完整记录，附文件的绝对路径
trove import ~/Pictures --into 参考图
trove tag <uuid> --add 动物
trove trash <uuid>                  # 软删除，可 restore；purge --yes 才永久删
trove analyze --limit 50            # 视觉模型读素材，回写描述 / 标签 / 评分；--undo 撤销整轮
trove duplicates                    # 按视觉签名分组的相似图片（签名之前的老资产不成组）
trove collection create 参考图
trove index status                  # 全文索引已收录条数、发件箱积压、谁持有写锁
trove doctor                        # 库、索引、ffmpeg 自检
```

共 20 个顶层命令，上面只挑了几个；`trove paths` 打印四类目录，其余在 `--help` 里。

约定只有三条：

- **stdout 永远是一个 JSON 文档**，`--human` 才换成表格；
- **退出码决定是否值得解析它**：0 成功、1 失败、2 用法错误、3 素材库不可用（不存在、schema 版本不符，或需要写索引但已被占用）；
- 诊断信息走 stderr，`--quiet` 只留错误。

`--help` 是完整的命令契约。库可用 `--library <slug>` 指定；`TROVE_DATA_DIR` / `TROVE_CONFIG_DIR` / `TROVE_CACHE_DIR` 可以把整个环境搬到别处跑。

---

## 构建与测试

```sh
cargo build
cargo test -p trove-core
cargo run -p trove-app
```

**当前测试基线：`trove-core` 650 + `trove-app` 61 全部通过；`cargo fmt --check` 干净；clippy 全工作区 0 告警。** `trove-app` 含 3 个真机 GPU 冒烟测试（EDL 眼罩光照、meshlet 剔除、按点云类别着色），无显卡的机器自动跳过；WGSL 与 uniform 布局的对齐另由 naga 在普通测试里静态校验，所以无头 runner 也挡得住着色器和 Rust 结构体错位。

各模块的实现细节按主题整理在 [docs/README.md](docs/README.md) 索引里。

---

## 许可证

[MIT](./LICENSE)

[gpui-kit]: https://github.com/panzhifu/gpui-kit
[rusqlite]: https://github.com/rusqlite/rusqlite
