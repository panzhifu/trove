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

[Trove 文档索引](docs/README.md) · [功能差距分析](docs/FEATURE-GAPS.md)

---

## 功能特性

### 组织与检索

| | |
|---|---|
| **全文搜索** | 基于 Tantivy，覆盖文件名 / 标题 / 描述 / 标签名，支持相关性排序、词语 / 子串 / 拼音三种匹配（`sunse` 命中 `Sunset`，`mao` 命中 花园里的猫），可叠加任意筛选条件 |
| **视觉搜索** | 以图搜图 + 按颜色搜索 —— 感知哈希 (pHash) + 颜色直方图，导入时自动计算，零推理依赖 |
| **智能收藏夹** | 规则驱动的虚拟文件夹，JSON 查询树，支持评分 / 类型 / 文本 / 标签 / 收藏 / 颜色 / 拍摄日期 / 宽高比 / 方向，`and` / `or` 组合，编译期校验 |
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
| **导入方式** | 只链接，不复制：文件留在你原来的位置，Trove 只记录路径与内容哈希。源文件移动或删除后可 SHA-256 校验重新链接 |
| **RAW / HEIC / SVG / PSD** | 相机 RAW 走 rawler 管线（去马赛克 → 白平衡 → sRGB），HEIC 经系统 heif-dec 转换；SVG 矢量渲染，PSD 合成内嵌预览 |
| **设计格式缩略图** | 挖掘 EXIF / 音频标签 / 字体族·样式·字重 / MP4 尺寸时长；有 ffmpeg 时自动生成视频海报 |

### 浏览与查看

| | |
|---|---|
| **三视图** | 网格（对齐布局）/ 列表 / 时间线，带密度缩放滑杆、多选 + 浮动工具栏 |
| **形状与比例筛选** | 工具栏形状筛选（横 / 竖 / 方）内置媒体人常用比例预设 —— 公众号封面 2.35:1、视频封面 16:9、短视频竖屏 9:16、照片 4:3 / 3:4、方形 1:1，3% 容差匹配圆整尺寸 |
| **检查器** | 缩略图 + 标签 + 色板 + 行内编辑（标题 / 描述 / 来源链接 / 评分），属性页显示 MIME / 大小 / 尺寸 / SHA-256，一键定位源文件 |
| **动图播放** | GIF / 动态 WebP / APNG 在预览与检查器中逐帧播放；网格缩略图保持静态以保证性能 |
| **字体实况预览** | 导入时用字体自身渲染「样张卡片」缩略图，样卡带家族名副标题；样张文字支持 `{name}` / `{family}` 占位符、可在设置自定义；检查器可把字体安装 / 卸载到系统 |
| **最近查看** | 左栏侧边最近 200 个资产，入回收站自动隐藏、恢复后回归 |

### 3D 模型预览

| | |
|---|---|
| **格式** | OBJ / STL / PLY / glTF / GLB 作为一等资产类型导入，带格式专属图标；`.blend` 经 Blender 无头转 GLB 后预览 |
| **GPU 视口** | wgpu 驱动，旋转 / 缩放 / 平移，双面 Lambert + Blinn-Phong 着色；无 GPU 时自动回退到 CPU 软件光栅器 |
| **观感** | 眼罩光照 (EDL) + 补洞 + 闭合网格背面剔除 |
| **大文件** | 流式常驻预算 + 抽稀，20 GB 不 OOM；覆盖度保持采样，缩放 / 移动全顺滑；离线空间索引可选（`.trovecloud`，9 B/点，60% 体积） |
| **网格剔除** | 大网格 (≥8192 面) 做 meshlet 聚类 + GPU 逐簇视锥剔除 |

### 视频与截图

| | |
|---|---|
| **无声预览** | ffmpeg 管线逐帧解码，播放 / 暂停 / 跳转 / 时间线，无音频管线 |
| **截图采集** | 全屏或交互式框选，直接入库 PNG；全屏走进程内捕获（xcap：Wayland wlr-screencopy / X11 / macOS / Windows），免 portal 弹窗；框选回落外部工具（grim+slurp / scrot / macOS screencapture），命令可自定义 |
| **批量格式转换** | 图片重新编码为 JPEG / PNG / WebP / BMP / TFT，可选长边限制，可重新导入转换后文件 |

### 维护与安全

| | |
|---|---|
| **回收站** | 删除 → 回收站 → 恢复 / 永久删除；清空回收站释放 blob 与缩略图 |
| **孤儿清理** | 移除不再被引用的 blob 文件 |
| **完整性校验** | 重算每个存储文件的 SHA-256 并与记录比对，问题资产一键入回收站 |
| **自动备份** | SQLite `VACUUM INTO` 快照到 `backups/`（每天至多一次，滚动保留 10 份） |
| **重复文件查找** | 按 pHash 聚类视觉相同的图片，每组「保留最新、其余入回收站」 |
| **存储占用统计** | 按目录列出 Trove 自己写进磁盘的东西（设置与主题 / 数据库 / 备份 / 缩略图与索引 / 日志 / 收件箱），标出哪些删掉只是花时间重建 |
| **多素材库** | 新建 / 切换 / 删除具名素材库；每个库有自己的数据库、监视文件夹与缩略图缓存，统计面板实时展示各类型数量 / 总容量 |

### 扩展

| | |
|---|---|
| **本地采集服务** | `http://127.0.0.1:23916`，`POST /add`（原始字节）与 `POST /fetch`（服务端抓取），浏览器扩展直连 |
| **浏览器扩展** | `extension/` 内置 MV3 扩展，右键发送网页图片到运行中的 Trove |
| **中英双语** | 设置 ▸ 关于 ▸ 语言实时切换，默认跟随系统 |

---

## 界面布局

| 停靠位置 | 面板 | 用途 |
|---|---|---|
| 顶 | 标题栏 | File / Settings 按钮、窗口控制 |
| 左 | 资源管理器 | 收藏夹树、智能收藏夹、最近查看、回收站 |
| 中 | 工作区 | 对齐缩略图网格 + 搜索 |
| 右 | 标签 + 检查器 | 标签筛选与资产详情 |

- **File** 菜单导入文件；**Settings** 打开设置窗口，六页：**关于**（版本 / 检查更新 / 语言）· **外观**（深浅模式 / 主题 / 自定义主题）· **文件**（存储占用 / 素材库 / 监视文件夹 / 缩略图 / 备份 / 清理与校验）· **模型**（点云观感 / 坐标轴 / 预览缩放）· **搜索**（全文索引 / 视觉指纹）· **快捷键**
- 还没有任何素材库时，启动进入**欢迎界面**而不是主窗口——素材库是记录与配置的落脚点，没有它主界面无事可做
- 工作区网格采用对齐布局（Google 相册风格），任意宽度下撑满面板
- 工作区标题栏内嵌弹出式搜索框，旁显示当前视图资产数
- 整窗都是拖放面；`Ctrl/Cmd+点击` 或 `Shift` 范围选择

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
    │   ├── dialogs/         # 设置六页、规则编辑器、重复文件查找、批量重命名、格式转换
    │   ├── panels/          # 资源管理器、文件夹、工作区、标签、检查器
    │   └── components/      # 预览组件（图片 / 视频 / 字体 / 3D / 音频 / 回退）
```

核心数据表：

```
assets             资产记录
collections        嵌套文件夹
asset_collection   资产-收藏夹多对多
tags               标签（不区分大小写）
asset_tag          资产-标签关联
smart_collections  智能收藏夹（规则过滤）
search_queue       全文索引发件箱：触发器入队，drain 喂给 Tantivy
view_history       最近查看记录（上限 200 条）
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

## 构建与测试

```sh
cargo build
cargo test -p trove-core
cargo run -p trove-app
```

**当前测试基线：`trove-core` 382 + `trove-app` 27 全部通过；`cargo fmt --check` 干净；clippy 全工作区 0 告警。** `trove-app` 含 2 个真机 GPU 冒烟测试（EDL、meshlet 剔除），无显卡的机器自动跳过。

对标同类软件的功能差距与路线图见 [docs/FEATURE-GAPS.md](docs/FEATURE-GAPS.md)。

---

## 许可证

[MIT](./LICENSE)

[gpui-kit]: https://github.com/panzhifu/gpui-kit
[rusqlite]: https://github.com/rusqlite/rusqlite
