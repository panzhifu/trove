# Trove ← Serpent 差距清单

> 对照物：`reference/Serpent`（上游 [dolag233/Serpent](https://github.com/dolag233/Serpent)）**v0.2.9**，Electron 43 + React 19。
> 本文：**v0.4.9** 的 Trove 与它在功能面上的逐项差距，以及明确决定**不追**的部分。
> **本轮（09-28）只有 Trove 动了，Serpent 一格没变**：上游仓库仍停在 **v0.2.9**，`src/` 仍是 **849 文件 / 258,255 行**、`MIGRATIONS` 仍是 32 段、末条 `version: 56`，与 09-27 那次复核逐字相同。所以 §2026-09-27 复核 那张表**不需要重审 Serpent 侧**，只需要把 Trove 这一轮的工作树改动记进去——记在 **§2026-09-28 复核** 那一节，并且**就地改掉了那一节里三行的状态**（音频卡片、检查器逗号输入标签、以及纵深索引升到的第 4、5 版）。Trove 这一轮动了什么、其中两件事**只做了一半就说自己做完了**，全部记在那一节里。
>
> **上一轮（09-27）两边都动了**：Trove 发了 **v0.4.9**（模型材质与共享 GPU 渲染器那一轮、智能集合同级拖拽重排、预览里方向键换素材、以及**卡片"活过来"的触发从指针定住改成按空格**——见 §C 的活卡片行与 [PREVIEW-SYSTEM.md](./PREVIEW-SYSTEM.md)），Serpent 从 v0.2.6 走到 v0.2.9。后者的增量单独成节，见 **§2026-09-27 复核**。
> 核验日期 **2026-09-28**（同一天连做十五轮，见 §2026-09-28 复核 与 第二~第十五轮；更早是 09-27 / 09-25 / 09-24 / 09-23）。两边的数字都取自源码而非各自文档：Serpent `src/` **849 文件 / 258,255 行** `.ts`+`.tsx`（与 09-27 逐字相同），**schema 仍是 v56 一格没加**——`MIGRATIONS` 32 段、末条 `version: 56`，`SUPPORTED_SCHEMA_VERSION = MIGRATIONS.at(-1)!.version`。Trove `crates/` **230 文件 / 117,414 行** `.rs`（第四轮加 `media/anim.rs`、第六轮加 `preview/anim.rs`）（09-27 记的是 224 / 110,221）。schema **v23**——v19 加了序列帧两张侧表、v20 加了 `source_path` 虚拟列与索引、v21 加了 `task_journal` 一张表、**v22 把没人读写过的 `ai_analysis` 删掉**、**v23 给星级加了守卫触发器并把存过的 0 折成 NULL**；`INDEX_VERSION` 从 09-27 的 3 走到 **5**（见 §D 纵深索引行）；包版本 **0.4.9**（本轮未 bump）；`en.toml` **804** 个叶子键；测试 **822 core + 78 app = 900** 全绿 3 ignored，`cargo test --workspace --all-targets` 实跑，`cargo fmt --all -- --check` 与 `cargo clippy --workspace --all-targets -- -D warnings` 两道门同轮一起过。
>
> **09-27 那一轮把上上轮三处"确认缺失"推翻了两处**，都是同一个失败模式——按 Serpent 的实现符号去 grep（`CF_HDROP`、`xdnd`、`file_drop`），命中零就记成"没有"，而没有按**能力**去问（"能不能把文件拖出窗口"）：
> ① **原生文件拖出早就有**（`2026-09-12` 的 `ce6a29d` 起，`panels/workspace/cells.rs:199-215` 网格格、`:494-510` 列表行，见 §A。上一版这里写的 `:192`、`:406` 已经漂走，本轮重数过）；
> ② **撤销栈上限已可配置**（`config.rs:56`、`:782-783`，见 §E）。
> 另一处是本文自己前后矛盾：§D 末尾那句"`prepare_cached` 仍零命中"写的时候 R3 后半已经做完了。三条都记在下面对应的行里。**09-28 这一轮犯了同族的一次**——不再是从 Serpent 的符号名推断，而是按**自己模块的注释**读、注释写什么就信什么，于是两件事被高估，见 §2026-09-28 复核。

图例：**量级** S ≈ 一天内 · M ≈ 数天 · L ≈ 一两周 · XL ≈ 月级。
「参照」列给 Serpent 的实现位置，读它比读它的文档快。

---

## 差距总账

| # | 域 | 差距 | 量级 | 判断 |
|---|---|---|---|---|
| A | 自动化与扩展（§A） | 无 MCP / 无脚本 / 插件编译期。**09-28 这一格收窄了一小块**：任务不再只活在内存里——schema v21 落了一张 `task_journal`，任务管理器每次状态迁移都写它，插件也能注册自己的任务类型（`TaskKind::Custom`）。**但"重启后续跑"这句还不能说**：读侧那个 `load_interrupted` 零调用方，见 §2026-09-28 复核 | XL | **最值得追**：Trove 已有 CLI 和 core/app 分层，缺的是把命令面形式化 |
| B | 同步与外部库（§B） | 无 WebDAV、无 Eagle/Billfish 迁移、归档只入不解、备份**能导不能还** | L | 追平即"能换过来"，是用户迁移成本的主要来源 |
| C | 内容类型 + 预览交互（§C） | 09-24 一轮收掉七块：文本查看器与编码探测、EXR/HDR/TGA、截当前帧入库、波形带拖拽、文本卡片、接触表生产者、活卡片即播与 seek（**09-25 触发从指针改成空格**）。**09-25 复核，剩的全在原地**：序列帧~~用户能碰的那一半一行都没有~~（**09-28 第二轮收了手动那一半**：门面五颗 + 右键成组/拆组 + `trove sequence` 三条，播放/轮播/导入后提示仍未做，剩 **L** 里的小半）/ 色彩管理的 ICC 那一半（`qcms` 仍未进依赖图，**M**）/ 文档真缩略图（**已定路线，决定不加依赖**）/ 文本内容入索引（**不再等 `INDEX_VERSION`——那个常量已涨到 5，缺的只是把正文喂进索引那一个字段**）。**EXR 曝光那一格降级**：`hdr.rs` 的曲线本来就吃 `stops` 参数、±10 的范围也写好了，缺的只是没有任何调用方传非零值——**现在补的是 UI，不是核**（见 §C）。**09-27 又拉开三格**（见 §2026-09-27 复核）：动画 GIF 走播放条（**L**，传输带被 `video.rs:90` 的 `kind != Video` 挡死）、PDF 兼容的 `.ai` 当图片看（**L**，顺带需要 Trove 一直缺的 PDF 光栅化）、RAW 头 0×0 被记成 1×1（**S**，`probe.rs:327`）。FLV / 0.25×–4× 倍速 / 旋转写回 Serpent 这轮才做，Trove 早就有。**09-28 又收掉 §2026-09-27 复核 那张表里的两行**：音频卡片可以在设置里选封面还是波形（`AudioCardStyle`，切换后靠"重建缩略图"生效），检查器输入框里逗号分隔的一串标签**按 Enter 就是追加**（以前 Enter 把整串当一个人名字，拆分挂在另一颗按钮上而且是**替换**），并多了一颗 Serpent 没有的"按层级链追加"按钮 | T7 的 UI 半 / T8 未动 / GIF+.ai 新增 | 每一项独立、都不大，但合起来是"能不能当日常素材库"的分水岭。AI 分析三类的"已打平"于 09-24 打折、同日 T6 补齐后又重新成立 |
| D | 检索纵深（§D） | ~~结果上限 2000~~ 与 ~~分页会话~~ 均已收敛；R1–R7 全部落地；**纵深索引走到 `INDEX_VERSION` 5**（09-27 到 3，09-28 再到 5）：元数据 facts 十个字段 + 四个**逐面拼音**字段（`tag:mao` 这种带限定的拼音检索以前必然落空，因为拼音只建在四面拼接上）+ 两个 `audio` 字段（`audio:` / `sample_rate:` / `channels:` / `bit_depth:` / `bitrate:` 五个限定词），带引号的多词短语现在**按位置匹配**（jieba 分词 + `PhraseQuery`，词序错了就不命中）。**09-28 另加一格 Serpent 没有的**：滤镜下拉里每个选项带当前筛选上下文下的**分面计数**（`PNG (42)`），见 §H | S | 大库可用性的地基，09-23/24/27/28 四轮已拆掉全部硬顶、冻成快照、并把元数据纵深推进索引 |
| E | 组织与元数据（§E） | 无持久化撤销(L)、批量改名不碰磁盘名(M)、忽略项不可还原(M)、无托管文件夹(未估，与"只链接"哲学冲突)、无标签合并(S)、无合集封面(S)。**09-28 在标签上补了一格 Serpent 没有的**：检查器里一颗按钮把逗号分隔的一串名字**建成父子链**（`风景,山,日落` 一次成三级），平级追加与层级链两种给法并存。**"无标签合并"这一格原样未动**。**09-27 这三格合流成一格了**：Serpent 这轮新做的文件夹检查器、链接文件夹树、侧栏文件夹快捷键全长在同一棵**磁盘文件夹树**上，而 Trove 侧栏只有合集树（`explorer.rs:902/804/826`）——要么一起立项(L+)，要么在 §H 明确写清"只链接"放弃了什么 | L + M×2 + S×2 | 「持久化撤销」是唯一的架构级改动，而它的**逆向配方其实已经有了**，缺的是落库 |
| F | 编辑与维护（§F） | 回收站无保留期(S) + 文件夹不能作容器入回收站(M)、单件缺图不自动补(**S–M**：便宜的是排一次任务，贵在渲染线程上不能同步解码)、zip 无预检(S)、备份不能还原(S)、更新只查不装(M)、无安装器(M)、库打不开无救援(M)、介质不自适应(M)、无障碍(L，且一半要推给上游 gpui) | S×4 + M×4 + L | 都是收口工作，不是新能力；「还原快照」这条尤其便宜——**校验和快照两样都已经在跑**（内容级 `plan_integrity` + 开库 `VACUUM INTO`），差的只是"从快照写回原位"那一个函数 |
| G | 平台与集成（§G） | 无单实例锁(S)、macOS 应用菜单细节(S)、7 份 locale 各缺 63–64 键(补文案 M，要过一遍全部语言) | S×2 + M | 截图与 Linux 发行这两项 Trove 反而领先，见 §H |
| H | 3D / 点云 | ~~只有 glTF / OBJ 的贴图与材质这一项（`Mesh` 无 UV）~~ **已追平**：`Mesh` 新增 `texture: Option<Box<TextureData>>`（含 UV / 贴图 / 金属粗糙度），glTF 与 OBJ 加载器均读取贴图与材质，WGSL 有完整的 `texture_2d_array` + `sampler` 绑定与纹理片段着色器 | — | Trove 全面领先，明细见 §H 表 |

---

## 2026-09-27 复核：Serpent v0.2.6 → v0.2.9 的增量

Serpent 在这两轮之间走了 **88 个提交**（其中 79 个非发布提交），版本号 0.2.6 → 0.2.9，`src/` 从 773 文件 / 249,024 行涨到 **849 文件 / 258,255 行**。有一点值得先说：**它的 schema 一步没动**——`MIGRATIONS` 仍是 32 段、末条 `version: 56`，和 v0.2.6 逐字相同（顺带修正上一版本文写的"56 段迁移"：是 32 段迁移**到达** v56，不是 56 段）。这 79 个提交里绝大多数是 `refactor(main/worker/preload): extract …`——把 `handleLibraryRequest` 那个巨型 switch 拆成 command handler，纯内部结构，不构成能力差。

按能力（不按 Serpent 的符号名——这是本文已经犯过两次的错）逐条对照，结果分三堆。

### 已经追平：Serpent 新做的，Trove 早就有

| Serpent 这轮做的 | Trove 的对应物 |
|---|---|
| 查看器左右旋转**写回图片文件** | 更强：Trove 无条件写回。`media/edit.rs:56 apply()` 按输入自身格式重编码，`library.rs:1201 batch_edit_images` 落盘并换入；不支持的编码器在 `edit.rs:146 is_editable_ext` 就被挡下。差别只在 Trove 没有那个设置开关 |
| 查看器支持**逆时针 90°** | `toolbar/title.rs:229` `preview-rotate-ccw` → `ImageEdit::Rotate270`，图标与文案齐 |
| 视频倍速改成 **0.25×–4× 可慢可快下拉** | `preview/transport.rs:33 SPEEDS: [f32; 9] = [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0]`，视频与音频共用，`atempo` 保音调 |
| 支持 **FLV** | `media/probe.rs:64` 的 `is_video_ext` 含 `flv`，`:152` 给 `video/x-flv`，并有测试钉着（`:484`） |
| 导入时不再把已有缩略图整批丢掉 | 结构上不可能：缩略图按内容哈希寻址且先查命中（`thumb.rs:40-42`），`remove_derived` 只按被清除的 sha 逐个走（`library.rs:1929`） |
| 库内拖拽不再弹导入提示、且能拖走整页选中 | 每个应用内落点都是**有类型**的 `drag_over::<AssetsDrag>`（`explorer.rs:793`），外部导入的壳根本不在这条路上；拖拽负载是选中集（`cells.rs:191-197`） |

### 新差距：这轮真正拉开的

| 能力 | Serpent | Trove | 量级 |
|---|---|---|---|
| **动画 GIF 用播放条控制** | 新增 `GifViewerPlayer` + `gif-playback-timeline`，GIF 与视频共用一条传输带，可拖可定位 | ◐ **核已于 2026-09-28 做完并量过；播放头那一半没有**。新模块 `media/anim.rs`：`FrameTimes`（每帧起始毫秒表 + `frame_at(ms)` / `ms_at(frame)` 双向映射，纯函数）与 `delays(path)`（按 magic bytes 分派，GIF / APNG / 动 WebP 三容器统一交帧延时）。**这一格原来的两个前提都不成立，已换成实测**：① 零新依赖、零外部进程——`image` 0.25.10 对 `GifDecoder`（`codecs/gif.rs:426`）、`ApngDecoder`（`png.rs:514`）**以及 `WebPDecoder`（`webp/decoder.rs:104`）** 全都实现了 `AnimationDecoder`，而 `png`/`gif`/`webp` 三个 feature 在 workspace `Cargo.toml` 里本来就开着；上一版那句「APNG 顺带能白拿，**WebP 不能**」里 WebP 那半句是错的。② 端到端量过：用 `gif` crate 写一张真实动 GIF（80/120/40/200 ms）再读回来，逐帧延时**无损往返**（40 ms 在百分之一秒单位下是 4 而不是 0，我第一版断言就按 0 写、被自己的测试驳回了）。**仍然缺的只有一件，而且它是真缺**：`media::anim` 目前**没有任何调用方**——`preview/video.rs:90` 那句 `data.kind != AssetKind::Video` 一行没改，播放条到 GIF 仍不通。见 §2026-09-28 第四轮。（**同日第六轮推翻了这条的一半**：`media::anim` 现在有调用方了，`preview/anim.rs` 用自己拥有的时钟驱动主区预览，动图不再靠 gpui 碰巧重绘——「按播放条那格的核」已经通了。上面那句「没有任何调用方」按原样留着，因为它正是那一轮记下的账。真缺的只剩传输带本身：`video.rs` 那句 `kind != Video` 仍未改，可拖可定位还没做。）| ~~**L**~~ **剩 S**（只差传输带接进来：`frame_at` 换帧重绘，帧表已在手） |
| **PDF 兼容的 `.ai` 当图片看** | 嗅前 1024 字节的 `%PDF-`，命中就按图片显示 | **无**。`.ai` 只出现在"用外部程序打开"的分派里（`open_with_apps.rs:35`），`probe.rs:96-108` 的图片臂与文档臂都不含它 → 落 `AssetKind::Other`，无缩略图无预览。更根本的是 **Trove 没有任何 PDF 光栅化路径**（无 pdfium，PDF 只有文本视图） | **L**（先要一个 PDF→位图的依赖，这条同时是"文档真缩略图"那块欠的） |
| **文件夹检查器**（对齐资产检查器的字段 + 封面 / 数量 / 大小） | 新增 `folder-inspector`：封面拼贴、名称、路径、资产数、子文件夹数、总大小 | **无**。检查器只认资产：`inspector.rs:303` 的入口是 `ctl.primary()`，而它读的是选中集里最后一个**资产 id**（`library/controller.rs:981`）；浏览合集时没有任何右侧详情。合集的字节数与封面在 `store/` 里也没有查询面（唯一的 `SUM(size_bytes)` 是全库的，`store/stats.rs:62`） | **L** |
| **忽略规则面板**（内嵌草稿预览，点保存才写入） | `.serpentignore` 编辑器 + 预览 | **无**。Trove 只**读** Git 的忽略文件（`tasks/ignore.rs:46 OWN_IGNORE_FILES = [".gitignore", ".ignore"]`），从不写，也没有编辑面板。但**草稿-修订-保存这个形状已经有了**：`dialogs/rules.rs:12` 的 `RuleDraft` 就是"这里什么都不写" | **L**（新存储 + 写路径 + 对真实扫描的预览；UI 骨架可复用 rules） |
| **拖文件夹到库根 = 建一个链接文件夹** | 链接树里出现该文件夹节点，规则/扫描都挂在它身上 | **部分**。拖入确实**不复制**（`library/jobs/import.rs:104` 注释即"用户导入是链接"），但落下去是**逐个文件的 `Origin::Linked`**，文件夹身份只剩 `facts.source_path`，靠 `panels/folders.rs` 事后归组 | **L** |
| **侧栏文件夹快捷键** | 新增 | **无**。`Explorer` 上下文只绑了一个 `Cancel`（`explorer.rs:719-720`） | **M** |
| **搜索历史**（回呼以前输入过的查询） | 上限 **24** 条、最新在前、大小写不敏感去重并保留新拼法、忽略空串，按 `libraryId` 分键存 localStorage（`src/renderer/search-history.ts`，`SEARCH_HISTORY_LIMIT`） | ✅ **已于 2026-09-28 第三轮做完**：存 `LibraryConfig::search_history`（每库一份 `library.json`，与它的 `libraryId` 分键范围一致），规则逐条对齐（24 / 最新在前 / Unicode 折叠去重 / 空串不记），提交点是**回车**——参照实现自己就写着"只记 settle 的查询，不记逐字符前缀"，所以这条选择不是折中而是照抄。列表挂在已有的 `search-popover` 输入条下面，点一条**既回填输入框也立刻执行**（能改而不是只能重复），带一颗清除钮 | ~~**S**~~ ✅ |
| **乱序 / 随机排序** | 修好虚拟画布上的乱序生效 | **无**。`AssetSort` 只有 CreatedAt / UpdatedAt / Name / SizeBytes / Rating / Duration / Color（`model/query.rs:12-28`），全仓无 random/shuffle 排序码 | **M**（难点不是 SQL，是"追加一页不能重新洗"这条不变量，`store/browse.rs:1086`） |
| **内嵌元数据显示** | 新增 `embedded-metadata.ts`，归一 15+ 字段成行显示 | **部分，而且差得很便宜**。抽取是真的（`media/metadata.rs:163` exif-rs 已挖出 Make/Model/ISO/FNumber/FocalLength/ExposureTime/GPS），但**挖出来的 `facts.photo` 在 `crates/trove-app/src` 里零引用**——一块已经躺在库里的数据没上界面。没抽的：视频/文档/归档（`metadata.rs:67` `_ => MinedMetadata::default()`）；没有 XMP/IPTC **读**（`library/jobs/xmp.rs` 是 sidecar **导出**） | ~~**S**（先把已挖的显示出来）~~ **已于 2026-09-27 做完**：检查器属性页新增相机 / 拍摄参数 / 拍摄时间 / GPS 四行，全部 `when_some`，无 EXIF 的文件一行都不出现。**过程中挖出两个真 bug**（`display_value()` 给 ASCII 加引号，导致 `captured_at` 对每张照片都是 None——时间线一直在按导入日期排序；快门被写成 `0.016666666666666666s` 而不是 `1/60s`），并补上一份真 EXIF JPEG 的内联夹具做端到端回归。**剩下的洞是回填**：`plan_remine` 只扫 `[Audio, Font]`（`services/maintenance.rs:99`），图片永不重挖，所以已入库的照片补不上这些字段（新导入的完全正常）；补它要动 `AssetPatch`（没有 `captured_at` 字段）+ 强制按钮文案。全打平（视频/文档标签抽取、XMP 读、custom tags）仍是 **L** |
| **检查器逗号一次输入多个标签** | 输入即拆成待提交 chip，Enter 全给 | ~~**部分**~~ ✅ **已于 2026-09-28 收平**：Enter 现在就是"拆分并追加"（`inspector.rs` 的 `append_tags_flat`）。本节下面记的旧形状——拆分挂在 `replace_tags_from_input`（`:136`）上而且是**替换**整组、Enter 走 `add_tag_from_input`（`:111`）把整串当一个人名字——已经不存在了。见 §2026-09-28 复核 | ~~**S**~~ ✅ |
| **音频卡片可选封面或波形，切换重建缩略图** | 新增设置项 | ~~**部分**~~ ✅ **已于 2026-09-28 收平**：`AppConfig::audio_card_style` 就是这个设置项，缩略图策略不再硬编码（旧文案指的 `thumb.rs:716` 那条硬规则已被两臂取代），重建走现成的"强制重建缩略图"。见 §2026-09-28 复核 | ~~**S**~~ ✅ |
| **从硬盘删除走当前主题的确认框** | 新增 | ~~**无，而且这条是数据安全的**~~ ✅ **已于 2026-09-28 第五轮做完，而且做的比这一格写的多**。这一格原来只数了右键那一条；实测**有四条**不可逆路径全都在单击即执行（见 §2026-09-28 第五轮）。共用闸门 `panels/workspace/mod.rs::confirm_destruction`，样式照 `title.rs:340 confirm_write_back`（`Danger` 主按钮 + `close_button(false)` + `Rc` 包回调） | ~~**S**~~ ✅ |
| **RAW 头 0×0 不要记成假尺寸** | 新增 `usablePixelDimension` 判掉占位 0 | **部分**。光栅这条路结构上免疫（Trove 根本不读 EXIF 维度标签，尺寸取解码器自己的帧头 `probe.rs:286`），但 RAW 那条 `probe.rs:327` 写的是 `raw.width.max(1)`——**把 0 变成 1×1 记进库**，正是 Serpent 刚关掉的那类撒谎 | **S** |
| **多文件原生拖出** | 整页可拖 | **部分**。应用内拖的是选中集，但拖出窗口时只解析被点那一个文件（`cells.rs:215` 单条 `FileDragPaths`） | **S** |

### 一个结构性观察

上面 **文件夹检查器 / 链接文件夹树 / 侧栏文件夹快捷键** 三行其实是**同一个洞的三面**：Trove 的侧栏只有合集树（`explorer.rs:902 collection_row`、`:804 recent_row`、`:826 trash_row`），**没有磁盘文件夹树**。Serpent 这轮在文件夹上叠加的所有能力都长在那棵树上是自然的；Trove 要单独补其中任何一格都会别扭。所以这三条不该分开排期——要么按"给 Trove 加一棵链接文件夹树"一起立项（L+），要么明确决定不做（那 §H 里"只链接、不托管"的哲学就得写清楚它放弃了什么）。

另外**「从硬盘删除无确认」这一条我建议不按差距排期，按缺陷排期**：它不是能力缺失，是少了一道本该有的门，而代价是不可恢复地删掉用户的源文件。

---

## 2026-09-28 复核：这一轮只有 Trove 动了

Serpent 侧一个提交都没有：上游仓库仍是 **v0.2.9**，`src/` 849 文件 / 258,255 行，`MIGRATIONS` 32 段、末条 `version: 56`，与 §2026-09-27 复核 那节记录的逐字相同。所以本轮不需要重审 Serpent，只需要把工作树里的 Trove 改动记进这张表。**本文所有 Trove 数字取自工作树现状，`cargo test --workspace --all-targets` 实跑：782 core + 70 app = 852 全绿、3 ignored。**

### 收掉的差距

| §2026-09-27 复核 那表里的行 | 现在的实现 | 状态 |
|---|---|---|
| **音频卡片可选封面或波形，切换重建缩略图**（S） | `AppConfig::audio_card_style` → `AudioCardStyle::{Cover, Waveform}`（`config.rs:579-608`），`thumb.rs` 的 `write_audio_cover`（`:721`）/ `rebuild_audio_cover`（`:739`）两臂各按这个偏好决定**先试哪一个**，另一个兜底；设置页「缩略图」组一颗开关（`dialogs/settings/files.rs:44-58`），旁边就是已有的"强制重建缩略图"，所以切换后**重建机器不用新写**。`ensure`（导入热路径）仍然只认**已缓存**的包络，`regenerate` 才付 ffmpeg——这条约束没动 | ✅ 已平 |
| **检查器逗号一次输入多个标签**（S） | Enter 现在走 `append_tags_flat`：按 `,，;；` 拆、逐个 `ensure_tag` + `tag_assets` **追加**。以前 Enter 走 `add_tag_from_input`，把整串当一个人名字；拆分逻辑在，但挂在 `replace_tags_from_input` 上、而且是**替换**整组 | ✅ 已平 |
| 上面两条没覆盖到的 | 多了一颗"层级链"按钮 `append_tags_chained`：把 `a, b, c` 建成 `a → a/b → a/b/c`，每级的父标签就是前一个名字。**Serpent 没有这个**，它只有"输入即拆成待提交 chip" | Trove 多出来的一格 |

### 只做了一半，但代码注释写成了做完了

> **这一节写的四件事，同日第二轮已经收掉了三件**（序列帧门面、journal 读侧、重试与优先级、`task_kinds`），现状见 §2026-09-28 第二轮 那张表。留着原文是因为**它记的失败模式比它记的事实更有价值**。

这一节是本轮最值得记的东西，因为**这份文档已经为同一个失败模式自我纠正过两次**（09-27 那次是按 Serpent 的符号名 grep、命中零就记"没有"；本轮是按自己的模块注释读、注释写什么就信什么）。两件事：

**① 任务持久化：写侧齐、读侧为零。** `store/task_journal.rs`（185 行）+ schema **v21** 一张 `task_journal` 表（12 列，`task_id` 主键）。`TaskManager` 每次状态迁移都写：`start` 落一行 `running`、完成/失败/取消落终态、重试 `retry_count += 1`；`library.rs:320-328` 在开库时**另开一条 SQLite 连接**挂上去（`Store` 的主连接是 `Rc<RefCell<…>>` 线程受限的，WAL 允许两条共存），所以这条是真的在跑，不是写了个函数没人调。**但是 `load_interrupted()` 全仓零调用方**——它读的就是"上次退出时还在 running/paused 的任务"，函数体写好了、注释还指着它说"On library open, `load_interrupted` reads tasks…"（`task_journal.rs:5-8`），而开库路径上没有一处调它。所以 §A 那行"重启不续"的**后半句仍然成立**：现在能回答"上次有什么被打断了"，还不能**把用户带到那件事面前**。这正是 §B 里"备份可还原"那一格记过的形状（写侧齐、读侧为零，差一个函数），本轮又造了一个。

**② 任务管理器的新 API 有四个没有生产调用方。** `RetryPolicy` / `start_with_retry`（每次重试从一个 `factory` 取一个新闭包、带 backoff、失败时发 `TaskEvent::Retrying`）、`TaskPriority` + `start_with_priority`、`Plugin::task_kinds()`（插件声明自定义任务类型，journal 用 `plugin:<name>` 前缀存所以能原样读回）。全仓 `grep -rn "start_with_retry\|RetryPolicy\|start_with_priority\|plugins::task_kinds"` 在 `tasks/mod.rs` 与 `plugins.rs` **之外零命中**（`TaskPriority` 只被 `start` 以默认值 `Normal` 间接用到）。**已经真的在跑的是两件**：`TaskPool`——所有任务改跑在一个共享线程池上（`available_parallelism().max(2)` 个 `trove-pool-N` 工作线程），以前是**每个任务一条线程**；以及**进度事件合流**——每个任务只保留最新一条 `Progress`，`MAX_EVENTS_PER_JOB = 128` 那道上限从此只约束终态事件，所以"一个没人看的任务把别人的 Completed 挤出队列"这件事结构上不可能了（`progress_events_are_lossy` 钉着）。这两件是本轮唯一有测试、也有调用方的改动，别和上面四件混着算。

### 纵深索引从 v3 走到 v5（解锁的是三件不同的事）

`INDEX_VERSION` 5（`search.rs:60`）。注释里逐版记着：3 = 元数据 facts，4 = 逐面拼音，5 = `audio` 字段。

| 加了什么 | 为什么它原来做不到 |
|---|---|
| `name_pinyin` / `title_pinyin` / `desc_pinyin` / `tags_pinyin` 四个字段 | 拼音与缩写以前**只建在四面拼接的一份索引上**，所以 `tag:mao` 想按拼音命中只能放弃——从拼接的拼音里答上来，可能命中的是文件名里的"mao"，「一个看不到原因的错答」。`expression.rs:40-42` 的模块注释原本就把这句写成有意为之。现在每个面自己有一份拼音，**限定词找回了拼音**，注释也跟着改成实话 |
| `audio_words` / `audio_tri` + `Target::Audio` + 五个限定词 `audio:` `sample_rate:` `channels:` `bit_depth:` `bitrate:` | 音频规格（采样率 / 声道 / 位深 / 比特率）09-27 已经进了**复合** facts 字段，所以无限定词能查到；但没有自己的面，就无法问"只按采样率匹配" |
| 带引号且含空格的短语走 `PhraseQuery`（`search.rs:1212-1263`） | 以前引号里的短语和散词走同一条路：各词独立命中、拼起来就行，词序错了也答应。要走位置匹配就得存词位置——**所有 jieba 分词字段因此从 `WithFreqs` 换成 `WithFreqsAndPositions`**（`indexed_text_with_positions`，`search.rs:367`），这是一次格式变更，也就是版本要涨的真正原因。分词只得到一个词时退回普通检索（没有位置可约束），n-gram 那一路仍然作为 `Should` 并进去，所以"部分匹配"没有丢 |

**本轮没有为这三件事写任何测试。** 现有 `a_field_qualifier_reaches_one_surface_only` 用的是 ASCII 词 `zong`，走的是普通限定路径；没有一个测试索引过 `facts` / `camera` / `audio` 任一字段，也没有一个测试查过 `*_pinyin`，短语那个测试（`a_quoted_phrase_finds_the_substring_it_describes`）本轮未动。**09-27 记"777 条 core 测试全过"为纵深索引背书过一次，那批测试同样是零**——这是这份文档 §I 专门在防的事（"文档为代码背书，代码没被证过"），本轮把它记下来而不是再犯一次。要补的最小形状：一份 `FactTexts` 非空的夹具 + 三个断言（`camera:Canon` 命中、`title:mao` 不命中"文件名里才有 mao"的那条、带引号两词短语在词序颠倒时不命中）。

### 本轮加的两格 Serpent 没有的能力，以及一个没量的代价

**分面计数**（详见 §H 那行）：`store/facets.rs` 把七个维度的 `GROUP BY` 接在列表自己那条 `WHERE` 上，工具栏六颗筛选器的每个选项显示成 `PNG (42)`，排名型列表只统计冻在会话里的那批 id。**代价本轮没量**：一次"全新列表"要跑七条 `GROUP BY`（tag 那条把整条 `WHERE` 包成子查询再 join 两跳），而 `docs/PERF-VS-SERPENT.md` 那份对照是在这之前测的。追加一页时沿用上一轮的计数（`panels/workspace/mod.rs` 的 `extends` 分支带着 `previous.facets` 走），所以翻页不重付；重付的是换视图、改筛选、以及不要求重数总数的那一轮。**这是这份文档一直在防的那类事——新加的查询没进基准就写进表格**。要补的形状：`serpent_parity_bench` 里给 `collectionSwitch` / `folderSwitch` / 首页三档各加一条 `withFacetsMs`。

**构建档**：`Cargo.toml:4-8` 从 `lto = "thin"` 改成 **`lto = true` + `codegen-units = 1`**（fat LTO）。它不改任何能力，但**让 `docs/PERF-VS-SERPENT.md` 里那两句构建描述过期**（`:58` 与 `:197` 都还写着 thin LTO），下一次 `bash bench/run-all.sh` 重跑出来的数字就会和表头的描述不一致。已在 §I 记一行。

### 本轮重新逐条 grep 过、状态未变的行

（标题里的"未变"只对这一轮当时成立：下面十条里有序列帧与搜索历史两条在**同日第二、第三轮**被做掉了，已在原句里就地标注；其余八条到 09-28 第三轮结束时重跑仍然成立。）

`media/probe.rs:327` 仍写 `raw.width.max(1)`——RAW 头报 0×0 时**记成 1×1 存进库**；`Cargo.toml` 无 `qcms`，`crates/trove-core/src` 里 `icc` / `qcms` 按词边界扫零命中（§C 色彩管理 T8 未动）；`grep -rn sequence crates/trove-app/src/` 当时仍只有两条无关注释（序列帧用户能碰的那一半一行没有）——**这句已被同日第二轮推翻**，`panels/workspace/context_menu.rs:236-296` 现在有创建与解散两颗；`model/query.rs` 无 random/shuffle（乱序排序没有）；`panels/search_box.rs` 当时无 history（搜索历史没有）——**这句已被同日第三轮推翻**，那里现在有 23 处 history 引用；`components/preview/video.rs:90` 第一句仍是 `if data.kind != AssetKind::Video { return None; }`（动画 GIF 到不了播放条）；`context_menu.rs` 与 `toolbar/selection.rs` 的「永久删除」当时仍直接 `purge_assets(&ids)` **没有任何确认**——09-27 建议把这条按缺陷而非差距排期，本轮它仍是缺陷；**同日第五轮已修，且发现不可逆路径其实是四条而不是两条**；原生拖出仍只交出指针下**一个**文件（`cells.rs:210-215`、`:505-510`，闭包签名 `_: &AssetsDrag` 把选中集丢在参数里）。

---

## 2026-09-28 第二轮：把四件"写好了没人调"的事接上

Serpent 侧仍然一格没动（v0.2.9 / 849 文件 / 258,255 行 / schema v56）。这一轮全部是 Trove 的收尾工作，对象就是 §2026-09-28 复核 里点出的那四件——**每一件的函数都在、测试可能也在，但没有任何调用方**。四件全部接上了，`grep` 逐件复验过；测试从 782 core 涨到 **791 core**（+9 条，见下表最后一列），全绿。

| 那件"没人调"的事 | 现在接在哪 | 还差什么 | 新测试 |
|---|---|---|---|
| **序列帧的门面** | `Library` 加了五个方法：`create_sequence` / `dissolve_sequences` / `dissolve_for_assets`（**按选中的资产**解散，因为用户在网格里选的是帧不是 run）/ `set_sequence_fps` / `sequence_of`（`library.rs` 的 `// -- image sequences` 一节）。界面：右键菜单两颗，「创建图像序列」只在**锚点是图片、选中数 ≥ `MIN_FRAMES`、且锚点不在某个 run 里**时出现，「解散图像序列」只在锚点属于某个 run 时出现；所有规则仍归 store，拒绝的理由走状态栏 notice。CLI：`trove sequence create --fps 24 <UUID…>` / `dissolve <UUID…>` / `fps <SEQ_UUID> <FPS>`。**默认帧率 `media/sequence::DEFAULT_FPS = 24.0`** | 差距表里 T7 剩下的三样一样没做：**导入后逐组提示**（`media/sequence::detect` 仍是纯函数、没有调用方）、**卡片轮播**、**序列播放器**。所以"用户能碰的那一半"从 0% 变成"能手动成组 / 拆组 / 改帧率"，还不是"能看到它在放" | `sequences_can_be_grouped_ungrouped_through_the_facade`：按顺序钉住四条拒绝的理由（帧数不足 / 跨目录 / 帧率越界 / 已属于别的 run）、成员位置按文件名里的编号而不是选择顺序、`dissolve_for_assets` 拿隐藏帧也能拆掉整组 |
| **任务 journal 的读侧** | `Library::assemble` 在开库时、把连接交给 `TaskManager` **之前**调 `load_interrupted`，结果存进 `Library::interrupted`；`interrupted_tasks()` 是读口，`clear_interrupted_tasks()` 配合面板的"忽略"按钮。**界面**：状态栏任务面板顶部多一块分隔开的"上次退出时中断"区，每行显示标签 / 类型 / `done`/`total`。摘要按钮现在也把它算进"需要看一眼"，否则一次崩溃之后状态栏写着"空闲" | **不提供"重试"**，这是有意的而不是没做完：journal 记的是一个任务是什么、跑到哪，**从不记启动它的输入**，而那些只存在于启动它的那个会话里（`Retryable` 在 controller 内存里）。所以能诚实说的是"你的导入停在 1200 里的 320"，不能替用户决定再跑一次。`task_journal.rs` 的模块注释已按这个事实重写 | 3 条：running→可读到中断→落终态后消失；retry 累加在同一行而不是分叉；`plugin:<name>` 前缀原样往返，且**这轮不认识**的 slug 也照常报出而不是丢掉 |
| **`RetryPolicy` / `start_with_retry` / `start_with_priority`** | 三个都有了真实调用方。**重试**：`Library::start_embedding_backfill`（Low + 2 次）、`start_ai_analysis`（2 次）、`start_ai_analysis_undo`（2 次）、app 侧 `jobs::start_import_job`（High + 2 次）。**优先级**：导入与模型解析 `High`，嵌入回填 `Low`，其余走 `start` 的默认 `Normal`。 | 没接的：`tasks/ai_analysis.rs` 等作业**内部**的逐资产重试（那是另一件事，现在仍由"下次再跑"承担）；调度本身仍是 FIFO，优先级目前只决定 `snapshot()` 的顺序 | 4 条：未声明的 `Custom` kind 被拒 / 声明后放行 / 内置 kind 永不受这道门约束；重试真的**重取工厂**并先报 `Retrying` 再 `Completed`；耗尽预算后以最后一次错误失败而不是静默；`snapshot()` 把 High 排在 Low 前 |
| **`plugins::task_kinds()`** | `Library::assemble` 现在把 `plugins::task_kinds(&config.disabled_plugins)` 喂给 `TaskManager::declare_task_kinds`，**并且这道声明是有牙的**：`start` 与 `start_with_retry` 都会把 `TaskKind::Custom(name)` 对着这份表验一遍，没声明过的直接 `StartError::UndeclaredKind` + 一条 warn。任务面板的标签也随之改准：插件 kind 显示成「插件任务 · <注册名>」而不是笼统一句 | `builtin::SidecarNotes` **仍然一个 kind 都不声明**——它只有导入管线的一个 stage 和一颗命令，没有该在后台跑的活，硬造一个是假需求。所以这条路径的**唯一使用者目前还是一个测试插件**。等真有插件后台任务时它已经能用了 | 1 条：关掉一个插件就不能再声明它的 kind |

两处顺带修掉的**注释性失真**（都是 §2026-09-28 复核 记过的同一形状）：`store/task_journal.rs` 的模块注释原本承诺"开库时读回来、界面就能恢复或重试"，`plugins.rs::task_kinds` 的注释原本写着"UI 用它填任务面板的筛选器和设置页的插件列表"——两句都不成立，现在都改成了实际成立的说法。

**这一轮没解决的**：§I 里 `inspector.replace_tags_hint` / `replace_tags_failed` 那两个孤儿键仍在九份 catalog 里；`PERF-VS-SERPENT.md` 的 `:58` / `:197` 两行仍写着 thin LTO；分面计数的成本仍未进基准。**新添的一处**：序列帧的右键项与"上次中断"区各用了 4 条和 2 条新 locale 键（`workspace.create_sequence` 等，九份一起加，棘轮 allowance 未动），而 `docs/MEDIA-FORMATS.md` 与 `docs/TAG-COLLECTION.md` 里关于序列帧的说法还没跟着这轮改。

---

## 2026-09-28 第三轮：搜索历史

Serpent 侧依旧一格没动。这一轮收 §2026-09-27 复核 里那条 S 级的**搜索历史**。

**存储选了 `LibraryConfig`（每库一份 `library.json`），没开新表。** 核过三件事才敢这么定：Serpent 自己按 `libraryId` 分键（`search-history.ts` 的 `Record<string, string[]>` + `clearSearchHistory(storage, libraryId)`），所以"每库一份"是参照行为不是取舍；`LibraryConfig` 已经在存同形状的有界列表（`watched_folders: Vec<PathBuf>`），`remember_query` / `clear_search_history` 照 `add_watched_folder` 那个先例写；`services/archive.rs:24` 的整包导出目录清单里就有 `library.json  per-library preferences`，所以它不会因为不是 SQLite 就漏出备份。**代价一条**：每日自动快照走 `VACUUM INTO` 只覆盖数据库，**历史进不了每日快照，只进显式整包导出**——要连快照一起带走就得回到开表那条路，为这个付一次迁移不值。

**规则逐条对齐参照实现**：上限 24、最新在前、**Unicode 折叠的大小写不敏感去重**（`ASCII` 折叠会把 `ÄNDER` 和 `änder` 存成两条）、空串不记、重复提交同一串是 no-op 不重写文件。提交点是**回车**，而且这不是折中：那份实现的头注释原话是"只记 settle 的查询，逐字符前缀不记"。

**一处要记的自错**：本轮动手前我在讨论里断言过"这个框子没有提交事件、`search_box.rs` 里连 `PressEnter` 处理器都没有"。那次 grep 的打印结果没带上匹配行，我就把"没显示"当成了"没有"。**`PressEnter` 一直在**（`search_box.rs` 的输入订阅里），提交点现成。这是同一个失败模式的第四次，且这次不是参照方的符号名也不是自家模块注释，而是**我自己的 grep 输出没看全就下结论**。

**回呼列表**挂在已有的 `search-popover` 输入条下面（那颗 pill 的样式一行没改），点一条**既回填输入框也立刻执行**——能改而不只是重复；带一颗清除钮；列表为空时整块不渲染，所以从没搜过的库看起来和以前一样。行内按字符截断到 40 再加省略号（按字节切会在第一个够长的中文查询上 panic，而只用 ASCII 写的测试恰好发现不了），完整串仍是执行的那一条。

**没做**：上下键走历史。那要新增一个可配动作（`keybindings.rs` 今天 21 条）+ 一份焦点归属决定，而点击回呼已经满足 §2026-09-27 那条能力描述。

测试 +3（`config::tests` 两条：有界/去重/持久/缺字段仍能读；`search_box::tests` 一条：截断按字符），总数 **794 core + 71 app = 865** 全绿；locale +2 键九份齐加，棘轮 allowance 未动。

---

## 2026-09-28 第四轮：动画图片的时间轴核（功能未完成）

**这一轮只解决了那个 L 的第一个未知量，功能本身没做完。** 记在这里是为了不让下一轮重新猜一遍。

阻塞问题是"帧表从哪儿来"。上一版的推断是**动 WebP 可能拿不到帧延时，要外部进程**——那是从 `panels/common.rs` 的旧注释（"gpui 原生解 GIF 与动 WebP"）推的，没查编解码器。查了：`image` 0.25.10 对 `GifDecoder`、`ApngDecoder`、**`WebPDecoder`** 三个都实现了 `AnimationDecoder`（`codecs/gif.rs:426`、`png.rs:514`、`webp/decoder.rs:104`），而 `png` / `gif` / `webp` feature 在 workspace `Cargo.toml` 里本来就开着。**零新依赖、零外部进程。** 所以那三种格式的帧表都能在同一套代码里拿到，"APNG 最容易、WebP 是例外"那个排序建议作废。

**落了什么**：`media/anim.rs`。`FrameTimes` 是一张纯数据表（每帧起始毫秒 + 双向映射 `frame_at(ms)` / `ms_at(frame)`），和任何解码器无关，所以边界能在没有窗口、没有像素的情况下钉死；`delays(path)` 是按 **magic bytes** 而不是扩展名分派的适配器，GIF 改名成 `.png` 也拿到它真实的帧表。契约照 `panels::common::decode_apng` 那个先例走 `Option`：静 PNG、JPEG、坏文件、单帧文件一律 `None`（意思是"退回静态缩略图"），不是错误——把最常见的情况报成错会让日志淹掉。

**量的结果**：测试用 `gif` crate 现场写一张真实动 GIF（80/120/40/200 ms）再读回来，**逐帧延时无损往返**。两个我自己写错、被测试驳回的地方记一下，因为它们都是"看着像对的算术/语义"：40 ms 在 GIF 的百分之一秒单位下是 `4` 不是 `0`（我按 `0` 写了断言）；零时长帧在 tie 里**不会被选中**，因为后一帧起始于同一瞬间且取最后者，而 `image` 的 `GifEncoder` 根本没有逐帧 API（只有 `write_image` 静图），夹具必须走 `gif` crate——这与 `exr` 那条 dev-dependency 是同一个理由，`gif` 0.13.3 也已在图里。

**没做什么，说清楚**：`media::anim` **零调用方**。`preview/video.rs:90` 的 `data.kind != AssetKind::Video` 一行没动，`image.rs:17` 仍是把文件丢给 gpui 循环播。所以**"GIF 能用播放条控制"这句今天仍然不成立**，这一轮只是把它从"要先决定帧表从哪来"变成"纯 UI 接线"。剩下的是 S–M：让 transport 接受动图资产、把解码出的帧持有住（`decode_apng` 那个 256 MB 预算可以照用）、seek 时按 `frame_at` 换帧重绘。

> **同日第六轮记（这条下面的"要么接上、要么不该留在树里"已经还上了）**：`preview/anim.rs` 现在就是那个调用方——持帧、按自己的时钟换帧，用的正是这一轮定的 `Option` 契约和那个 256 MB 预算。剩下 seek 与传输带仍没做，所以这一格还是 ◐ 而不是 ✓。见 §2026-09-28 第六轮。

**这一轮也提醒一件本仓库反复出事的事**：一个测试齐全、文档写好的新模块，如果没有调用方，它就是这份文档 §2026-09-28 复核 里点名的第五个"写好了没人调"。要么下一轮把 UI 接上，要么这个模块不该留在树里。

---

## 2026-09-28 第五轮：不可逆删除的确认框

差距文档从 09-27 起就建议把这条**按缺陷排期而不是按差距排期**——理由不是"能力缺失"而是"少了一道本该有的门，代价是不可恢复地删掉用户的源文件"。本轮做它。

**这一格原来数错了规模。** 09-27 那行只点了右键菜单一处。实测 `grep -rn "purge_assets\|empty_trash()\|trash_or_purge_selection" crates/trove-app/src` 之后是**四条**单击即永久删除的路径：

| 路径 | 位置 | 原来 |
|---|---|---|
| 右键「永久删除」 | `context_menu.rs` | 直接 `purge_assets` |
| 回收站工具条「永久删除」钮 | `toolbar/selection.rs` | 直接 `purge_assets` |
| **Delete 键**（回收站视图里同键走 purge） | `toolbar/selection.rs` + `app/root.rs` 的 `TrashSelected` 动作 | `trash_or_purge_selection()` 内部判 `showing_trash` |
| **清空回收站** | `toolbar/title.rs` → `interactions.rs::empty_trash` | 零确认，且是四条里删得最多的 |

**只堵其中两条比一条都不堵更坏**：用户从菜单学到"这个操作会问我"，就会假定快捷键也会问，于是更快地去按它。所以四条一起接同一个闸门 `panels/workspace/mod.rs::confirm_destruction`，共用一个 `purge_gated` / `trash_or_purge_gated`；Delete 键那条只在**回收站视图**里要确认（平时它是可逆的移入回收站，给可逆操作加门会把人训练成随手点掉）。样式照 `toolbar/title.rs:340` 已有的 `confirm_write_back`（`ButtonVariant::Danger` 主按钮、`close_button(false)`、`Rc` 包回调因为 dialog builder 是 `Fn` 而非 `FnOnce`），不是新造一套。

**警告文案分两种情形，而且这是本轮唯一有真判断的地方**：`purge_delete_sources` 关着时只说"N 个素材将永久删除，无法撤销"；**开着时**才追加一行"Trove 之外你自己的文件也会一并删除"。理由是每次都威胁最坏情况会把人训练成不看对话框——真正不可恢复的只有源文件那一半，库里的 blob 本来就能重新导入。为此把判断拆成纯函数 `purge_warning_for(delete_sources, count)`，这样它能在没有窗口、没有库、不改动用户配置的情况下被测（`the_source_file_threat_appears_only_when_the_setting_is_on` 钉住两分支不同且各含/不含第二行）。

**闸门覆盖性复核**：改完再 grep 一遍全 app，剩下的每一处直接调用都在闸门函数体内（`purge_gated` 的 on_ok、`trash_or_purge_gated`、`WorkspacePanel::empty_trash` 的唯一调用点），**没有绕过路径**。

**没验的**：对话框的实际外观（宽度、换行、Danger 配色）——GPUI 窗口在这里跑不起来，只能靠 `confirm_write_back` 是同一种对话框这一点来推断。计数为 0 时不弹（清空回收站在空库上直接返回）。测试 **800 core + 72 app = 872** 全绿，locale +3 键九份齐加、棘轮 allowance 未动。

---

## 2026-09-28 第六轮：动图真的在播了（接上第四轮那张账）

**症状是"进入预览界面 GIF 不动"**，而它不是没解码——gpui 解了，只是它换帧的时机不归我们。读 `gpui-pre 0.3.5` 的 `elements/img.rs:318-339`：帧推进写在 `request_layout` 里，条件是 `frame_count > 1 && !cx.reduce_motion()`，外面还套着一层 `window.is_window_active()`。两个后果都不是从"把路径交给它"那行代码看得出来的：**没有别的东西重绘这个窗口就不换帧**，以及**窗口失焦时彻底冻住**（`last_frame_time` 直接被清成 `None`，重新聚焦才重新计时）。用户能做的只有晃鼠标。所以这一轮换的是"谁决定什么时候换帧"，不是"能不能解码"。

**落了什么**：`preview/anim.rs`。一次解码（`trove_core::media::anim::decode`，就是第四轮那个核）成 N 张**各含一帧**的 `gpui_kit::RenderImage`，BGRA 交换在这里做掉而不是每次重绘做一次；一条自己的循环按墙上时钟决定 `frame` 是哪一张；`Drop` 时置 `alive`，循环随之退出（循环只持 `WeakEntity`，不会把面板留住）。形状抄 `video.rs` 的循环，理由是同一条：控制状态放 `Arc<Mutex<…>>`，循环永远不需要借 `App` 就能知道用户刚刚做了什么；每一帧的到期时间用 `Instant` 排程而不是"睡固定时长后换帧"，否则每次 notify 花掉的毫秒都被加进下一帧的延时里，看起来就是抖。

**三个决定，各自都有代价**：

1. **解码挪出 UI 线程。** 第四轮的核是"要拿帧表就得整份解码"，几百帧的 GIF 那是几百毫秒到一秒——在打开预览的那一下发生，等于把一坏掉的动画换成一个卡住的窗口。照 `video::load_player` 现成的样子加了一个 `anim_loading`：面板先显示它本来就在显示的静图，帧到了再换。**代价**：动图播放期间 `zoomable()` 为假，滚轮缩放与拖拽平移没了。这是本轮唯一的功能退让，说清楚它换掉了什么——原来的缩放走 `zoomed_still`，而那条路用的是 gpui 原生动画源，也就是**放大之后本来就不换帧**，是一个不动的高清图。视频、音频、文本三个舞台同样不缩放，所以现在是全库一致，只是 GIF 从"能缩、不会动"变成"会动、不能缩"。
2. **`wants_player` 按 mime 判，不按文件嗅探。** 用的是 `data.animated.is_some()`（`panels::common::animated_preview_source` 已经算好的那半份判断），因为这个 gate 在每一次预览的必经之路上，而"这张 GIF 到底有几帧"要整份解码才知道。判错的代价是一个后台任务答 `None`、静图照常显示；JPEG 与静 PNG 连这个任务都不会起。
3. **`Shared` 里只有 `playing`。** `seek` 与 `frame_at` 那一半本轮不写，因为没有播放条可以拖；写了就是这份文档点名的第 N 个"写好了没人调"。点击画面切暂停是眼下唯一控件，也是光标本来就在的地方。核心侧 `FrameTimes::frame_at` / `ms_at` 仍是 `pub`（trove-core 的公开 API，不会被 dead-code 门拦下），播放条那一步直接接。

**量的结果**：+2 测（`each_prepared_frame_is_one_bgra_image`、`a_single_frame_picture_prepares_nothing`），合计 **802 core + 74 app = 876** 全绿 3 ignored，`fmt` 与 `clippy -D warnings` 同轮过。回归性按老规矩证过一遍：把 `pixel.swap(0, 2)` 改成 `swap(1, 2)`，断言立刻失败在 `left: [1, 70, 40, 255]` / `right: [70, 40, 1, 255]`——通道序错位在这套代码里没有任何别的地方会说话。顺带记一个不指向原因的坑：测试模块里 `use super::*` 会把父模块 `gpui_kit::*` 里那个**名为 `test` 的属性宏**一起带进来，于是每个 `#[test]` 都报 "recursion limit reached while expanding `#[test]`"；本仓库其他测试模块都是具名 `use super::xxx`，所以从没撞上。改成具名 import 即解。

**没做的，逐条**：
- 传输带（播放条）仍不通：`preview/video.rs` 那句 `kind != AssetKind::Video` 一行没动，可拖可定位还没有。`FrameTimes` 已经在等它。
- **检查器卡片与缩放态仍走 gpui 原生那条路**（`image.rs:37` 的 `compact`、`zoomed_still`）。主区是本轮的范围；小卡片在滚动侧栏里被不停重绘，症状不明显，换它要另做一份帧缓存，不值。
- 一条遗留要说明白：`panels::common::animated_preview_source` 对 `image/png` 仍会在 `AssetPreviewData::from_asset` 的调用路径上**同步整份解码 APNG**（`decode_apng`），也就是说打开任何 PNG 预览都还在 UI 线程解一次。本轮没动它，因为它同时是检查器卡片和 `zoomed_still` 的数据来源；新的播放器只是不再需要它。这是"动图预览"这件事剩下的、唯一还在 UI 线程上的那份开销。

**没验的**：视觉上到底动没动——GPUI 窗口在这里跑不起来，像素层面的判断留给用户。有一条能自己确认的判别法：**把焦点切到别的窗口再切回来**。gpui 原来会在失焦期间冻住并清掉计时起点，新循环不归它管，回来时应当停在别处而不是第一帧。

---

## 2026-09-28 第七轮：数据结构的分层审计，以及 P0 止血

这一轮不是追 Serpent，是回答"这些数据结构是不是在反映领域"。审计结论 + 排好的计划记在一起，是为了下一轮不用重新推导一遍。

**审计的四问四答**（数字都是实测）：① 有没有独立领域实体——`model/` 目录是独立的，但 `Asset` 就是 `assets` 表那行（24 字段≈列，`store::assets::get` 经 `asset_from_row` 直接产出领域类型，中间没有行类型），所以 domain 与 db 是同一个结构体；② 充血还是 Service——贫血，`impl Asset` 只有 `file_stem()` 一个方法，业务在 `library.rs`（3,841 行 / 78 `pub fn`）和 134 个 `store/` 自由函数里；③ DB 模型有没有挡住接口层——没挡住：app 里 49 处 `library.store().conn()` + 65 处直接 `store::xxx::`，CLI 里 14 处 `conn()`，而 `trove-cli/src/ctx.rs:339` 的资产详情就是 `serde_json::to_value(asset)`，函数自己的注释写着"the stored row"；④ 有没有仓储抽象——只有具体类，`Store::conn()`（store/mod.rs:240）是 `unsafe { &*self.conn.as_ptr() }`，一扇可绕的门等于没有门。

**一个纠正自己的框架**：这不是"全库贫血"。275 个 `pub` 类型里 ROW 只有 16 个，129 个值对象全在 `media/`（3D/成像内部，与 SQLite 零耦合），面向库的只有 `model/` 34 + `store/` 13 + `library.rs` 7。真正的结构病是一句话：**16 个 ROW 类型里 10 个住在 `model/`，只有 2 个住在 `store/`**——域包持有行形状。要动的范围比"DDD 重构"小一个数量级。已经做对的先例也记下来，因为计划要照抄它们而不是另造一套：`EmbeddingSpace`（类型 / SQL CHECK / 解码器三者一致，全库唯一）、`NewEmbedding::validate`、`Appearance` + `Accent`（`migrate_appearance` 把自由 hex 折掉并**删列**，是"用类型替换字符串列"的完整范例）、`Page{total,items,truncated}`、`Listing`（browse.rs:377-395，想要的分层其实已经写好了，只是私有）。

**几条实测到的具体缺陷**（每条都在源码里读过，不是从注释推的）：`Asset` 的 `origin` + `rel_path: Option` 其实表达**三**态——`library.rs:1932-1947` 从导出档恢复元数据时会造一条"有名字没 blob"的记录，代码自己叫它 placeholder，注释写着"invisible to orphan cleanup until healed"，`media/import.rs:422` 负责愈合；`Linked` 那半同样无约束（`Linked ⇒ facts.source_path` 无人检查），`library.rs:491` 与 `media/thumb.rs:69` 都是 `?`，静默变 `None` 后被调用方报成"文件不见了"——**误诊**。`MAX_RATING` 只在 `model/asset.rs:254` 一扇门上查，读回时 `rating: v as u8` 截断（`assets.rs:631`）、`size_bytes` 用 `.max(0)` 静默修（:623），schema 对 `rating`/`kind`/`usage_status` 没有 CHECK（只有 origin/space/fps/position/frame_number 有），而 `parse_kind` 遇脏值让整份 `list()` 失败（:1157）。`ai_analysis` 表建了、索引了、独占一个升级步、还被版本探测查过——**全仓 0 处 INSERT/SELECT**，真数据住在 `facts.unknown["ai_analysis"]` 当无类型 JSON。`Store::conn()` 的 `# Safety` 注释说 RefCell 会在运行时保证没有可变借用，**这句不成立**：读路径从不取借用，而 :191/:213/:248 用 `borrow_mut()`，所以是静默 `&`/`&mut` 并存而不是 panic。
> 顺手记下两个我纠正掉的**误报**，因为它们都来自我自己派出去的审计：① "tag 名打错会编译成空 `IN ()` 匹配零条"——错，`ids.is_empty()` 走的是名字等值的 `EXISTS` 分支（`store/smart.rs:126-130`），真实缺陷是 `subtree_ids(...).ok()` 把 DB 错误吞掉后，一条"含整个子树"的规则**静默降级**成"精确同名"；② "`record_retry` 写 `finished_at = NULL` 所以 Completed+None 可达"——错，它同时把 status 写成 Running，是自洽的；那一处真正的问题是 `str_to_status` 的 `_ => TaskStatus::Failed` 把无法识别的状态读成 Failed。

**本轮决定（四条，已拍板）**：占位态**在类型里命名**（不打算靠"从源头消灭"）；`ai_analysis` **删表**；P1 关门**app + CLI 一起**（63 处）；本轮范围 **P0 + P1**。

**P0 落了什么**：① 任务日志的 9 处 `let _ = task_journal::…`（`tasks/mod.rs`）全部改走一个 `journal_write` 助手——失败就置 `journal_degraded` 标志并只记一条 warn（一个坏日志会在此后每次转移都失败，逐条 warn 会把那句唯一需要的话埋了）。标志由 `TaskManager::journal_degraded()` 暴露，**调用方是任务面板**：它原来会因"没有行"而读出"没有任务被中断"，这正是日志失败唯一无法被后来进程看见的代价，所以面板加了一条警告行（`journal_degraded_note`，新键 `task.journal_degraded`）。② 侧栏两处删除（托管集合 `explorer.rs:1243`、智能集 :1184）原来是 `let _ =` 吞掉错误**且无条件** `generation += 1` → 列表重读数据库发现集合还在，而用户已经看见一个空位说明它没了。改成 `context_menu.rs:330-345` 早就有的那个 Ok/Err 形状：成功才换视图、才 bump，失败走 `report_error`（新键 `explorer.delete_failed`）。③ 为此新增 `Library::delete_collection`，形状照现成的先例 `delete_smart_collection`（一行门面、不记 undo），并把"为什么不记 undo"写进文档：集合就是它的成员表，撤销删除要重建 undo 栈没有快照的行。④ 顺带纠正一处注释与代码不符：`TaskManager` 的文档声称 journal 锁"never held while `jobs` … are held"，实测两处 `record_start` 就在 `jobs` 持有期间发生——真实的锁序是 `jobs` → `journal`，且没有反向路径，所以把注释改成事实而不是改锁序（没有可观测的死锁路径，动它不属于止血）。

**量的结果**：测试 **804 core + 74 app = 878** 全绿 3 ignored，`fmt` 与 `clippy -D warnings` 同轮过，locale +2 键九份齐加、棘轮 allowance 未动（de 64/2、其余 63/1、zh-CN 0/0 实测照旧）。新测试的回归性按老规矩**用变异证明**：把 `journal_write` 的 `if let Err(error) = …` 换回 `let _ = run(&conn);`，`a_journal_write_that_fails_is_reported_not_swallowed` 立刻 FAILED，而同批另一条 `a_working_journal_and_a_missing_one_both_stay_clean` 仍过——后者存在的理由就是防止"永远置标志"这种作弊实现蒙过前者。

**没做的，逐条**：51 处守写的 `let _ =` 只处理了 11 处（9 journal + 2 explorer），其余 ~40 处里 17 处是 `config.save()`；`AppConfig::load()` 的 82 处不缓存、`toolbar_row` 在 render 路径里每帧 `read_to_string` 一次磁盘的事，本轮没动；`data.rs:387` 那条"持着 `&Connection` 跨越另一次 `conn()` 写"的可能撕开读，以及 CLI `open_read_only` 的连接实际仍可写——这两条是子代理报的，**我没有逐行复核，所以没写进依据**，P1 关门时自然会被逼到眼前。
> P1..P6 的排序、半径和门禁见本节末尾的"数据结构计划"卡片，下一轮直接从 P1 开始：**P1** `Store::conn()` 收 `pub(crate)`，63 处（app 49 / cli 14，散在 23 个 app 文件）改走门面，预计给 `Library` 补 15–25 个方法，那个 unsafe 从公开 API 的前提退成内部细节；**P2** `enum AssetLocation { Stored{rel_path}, Placeholder, Linked{source_path} }`（schema 不动，23 处 `Asset {}` 字面量全在 core、app 0 处）；**P3** `enum Placement { Live, Trashed(DateTime) }` 收掉 `trashed_at`(76)/`is_trashed`(24)/`in_trash`(20) 三种写法与 ≥9 处手写 `trashed_at IS NULL`；**P4** `Rating`/`ContentHash` + 补 SQL CHECK（配 v21→v22，顺带补上一直缺的迁移测试）+ `parse_kind` 改成降级那一行并报告；**P5** 智能集规则 `query: Json` → `SmartNode`（半径 core 24 + app 27，会碰规则编辑器）；**P6** `ai_analysis` 走 v21→v22 DROP。**明确不做**：Repository trait/mock、三层包重命名、把 129 个 `media/` 值对象卷进来、用 `Patch<T>` 泛型替换 6 个 `Option<Option<_>>`（CLI 5 处依赖具体形状）、给 `mime`/`ext` 造 newtype。

**没验的**：面板那条警告条的实际外观与措辞长度（GPUI 窗口在这里跑不起来）；`journal_write` 的 poisoned-lock 分支只有代码路径、没有测试（构造不出毒化的 journal 锁）。

---

## 2026-09-28 第八轮：P1 关门——`&Connection` 不再是公开能力

**做了什么**：`Store::conn()`（store/mod.rs:240）与 `Library::store()` 降为 `pub(crate)`。改动顺序是"先看见再改"：翻掉可见性 → 编译器一次性点出 **64 个**越界点（app 49 / cli 14，散在 23 个文件）→ 逐个改成一次门面调用 → 再翻回来验证门真的关上了（现在 `grep -rn "library.store()" crates/trove-app crates/trove-cli` 是 0）。为此 `Library` 长到 **111 个 `pub fn` / 4,088 行**（原 78 / 3,841）——这是上一轮拍板的取舍：门只有一扇，方法名是领域动词，比一把万能钥匙安全。

**顺带纠正的那条注释**：`Store::conn()` 原来的 `# Safety` 写着"RefCell 会在运行时保证没有可变借用存在"——**这句话不成立**，发引用时从不取借用标志，所以后来的 `borrow_mut`（:191/:213/:257）看不见它。注释现在说的是事实：真正撑着它的是"单线程、一次操作、持借用不回进 store"这条约定，违反约定的后果是同一个 `sqlite3` 句柄上并存 `&` 与 `&mut`（UB，不是 panic），而这正是门不再公开的理由。`unsafe` 本身留着——按上一轮的决定，它的影响面已经从"任何一层"缩到"一个模块"。

**两处诚实的第二连接**：`assets::duplicate_groups_at(db_path)` 与 `visual_search::search_by_image_at` / `search_by_color_at`——路径进、连接自己开自己丢，替掉原来"example/对话框借道 store 拿引用"的做法。同时新增 `Library::db_path()`，因为 `"library.db"` 这个名字原先在 app 里手拼了 4 处（`workspace_search.rs:52/83`、`settings/files.rs:680`、`duplicates.rs`）；库的文件布局不该是调用方的常识。

**必须点名的行为变化（性能工具）**：5 个 `crates/trove-core/examples/*` 原先借 `lib.store().conn()` 跑裸 SQL，现在各自 `rusqlite::Connection::open(lib.db_path())`。**含义**：bench 测的是同一文件上的**另一个连接**（WAL 允许），不再是应用自己那条。语句、schema、pragmas 都一样（都经 `Store::open` 建过 schema），但连接级的语句缓存与 busy 语义不再同源。`PERF-VS-SERPENT.md` 里那些数字因此应当在下一轮重跑后再引用。

**承诺"逐条报"的合并，一共三条，只做了两条**：① `dialogs/rules.rs` 保存路径去掉两次显式校验——`create_smart_collection` 内部本来就是同一顺序做这两步（`input.validate()` 然后 `smart::validate_json`），dialog 是在重复门面的事；② `settings/search.rs` 的签名回填不再把 `root: PathBuf` 带进 worker（`Library::compute_visual_signature(id)` 知道自己的 root）。③ `panels/common.rs` 的 `live_count` / `trash_count` 我**没有**合并：它们现在是 `query_assets(...).total`，换成 `assets::count(...)` 数字相同但查询从"分页 SELECT"变成"COUNT(*)"——这是一次性能改动而非改名，留到 P3 一起决定。

**本轮实测到的第 N 个"写好了没人调"**：`Library::evaluate_smart_collection`（library.rs:1046）除定义外**零调用方**——app、cli、测试全无。explorer 要的是徽标计数，用的是新加的 `count_smart_rule`。它 `pub`，所以 dead-code 门拦不住它。要么 P5 让规则编辑器真正用它，要么删。

**门**：878 测全绿（本轮**没有新增测试**——可见性变更的证据形式不是红测试，而是"把可见性翻回去就得到 64 个编译错误"），`clippy -D warnings` 与 `fmt` 同轮过；`cargo doc` 的断链 10 条**全部先前已有**（我今天新引入的两条已修）。

**没验的，说清楚**：本轮正确性靠的是"每个调用点换成同样一次查询、同样的错误处理"这一机械性质加上编译器，**没有任何一条测试覆盖"面板渲染出同样的数字"**——804+74 个测试里没有一个 UI 渲染断言。也就是说，如果我在某个调用点把 `unwrap_or(0)` 的时机挪错了位置，测试不会响。要复核就看侧栏计数、标签面板数字、文件夹列表和筛选器下拉这四块与改动前是否一致。

---

## 2026-09-28 第九轮：P6 删掉 `ai_analysis` 死表，并补上缺的两条迁移测试

上一轮拍板"删表"，本轮执行，因为它是这份清单里少见的**删比加风险低**的一格。

**做了什么**：`SCHEMA_VERSION` 21 → **22**。v21→v22 一步 `DROP INDEX IF EXISTS idx_ai_analysis_model; DROP TABLE IF EXISTS ai_analysis;`，并从"从零建形"的脚本里去掉这张表——也就是说这个 build 创建的库从来没有它。v14→v15 那一步**仍然创建它**：一步必须描述它当时代产生的那个形状，改成"后来被删了所以不建"会让链条中间那一段说谎。结果是一条走满链的库在 v15 得到表、在 v22 失去表，这正是链条该有的样子。

**顺带补的测试债**：`store/mod.rs` 里 v14/v15/v17/v19 各有命名的迁移测试，**v20→v21（任务日志）没有**——这个缺口是加日志读写测试那一轮留下的，本轮补上 `a_v20_library_gains_the_task_journal_and_keeps_its_rows`。新增的 v21→v22 测试断言三件事：本 build 新建的库没有该表、带着它的库会被删掉、以及在"已经没有东西可删"的库上重跑这一步依然走完。另把老的 v14 测试里 `assert_eq!(tables, 1, "the upgrade created the cache table")` 翻成 `0`，措辞改成"走完之后当前形状里没有缓存表"——原先那句在 v22 之后是在替一个不存在的形状背书。

**两条测试都做了变异核验**：把 `sql: UPGRADE_20_TO_21` 与 `sql: UPGRADE_21_TO_22` 换成空串，两条各自失败（前者在 `load_interrupted` 处报 `no such table`，后者在 `the table and everything in it are gone` 上 `left: 1 / right: 0`）。改回来全绿。**没走这一步，"我写了迁移测试"只是意图**。

**门**：**806 core + 74 app = 880** 全绿 3 ignored，`fmt` 与 `clippy -D warnings` 同轮过。分析结果本身不受影响：它一直住在 `assets.extra` 的 `ai_analysis` 标记里（`tasks/ai_analysis.rs` 的 `MARKER_KEY`），撤销路径读的就是那一处，那张表从来没人写过。

**下一格 P2**（`AssetLocation` 三态命名）与 P1 的区别要说清：P1 的 64 个点由编译器逐个点名，P2 改的是 `Asset` 本身——`origin` 57 处、`rel_path` 56 处、`source_path` 77 处、9 个构造点，且决定里包含"导出 JSON 字节兼容"。做法是先用一个 wire 影子结构把今天的形状钉成测试（round-trip 全字段相等，漏一个字段就红），再动类型。解码器必须是全函数，而 `Linked` 缺 `source_path` 只可能来自坏数据（`media/import.rs:440` 写侧总是填），所以类型给它第四个状态而不是撒谎——这一条是我定的，不在批准的选择之外，改名或去掉只动一处。

---

## 2026-09-28 第十轮：P2 —— 资产"文件在哪"变成一个值

**落了什么**。`AssetLocation` 四态：`Stored { rel_path }` / `Placeholder` / `Linked { source_path }` / `Unrecorded`。`Asset` 的两列 `origin` + `rel_path` 收成 **private**，唯一入口是 `Asset::from_seed(AssetSeed { location, .. })`（构造）与 `Asset::location()` / `set_location()`（读写）。14 个 `Asset { … }` 构造点（5 生产写者 + 行解码器 + 6 测试夹具 + 2 bench example）全部改走 `from_seed`，改完 `grep` 核实：**core 之外 0 处读写这两个字段，core 内除 `model/asset.rs` 也 0 处**。

**为什么是四态而不是三态**。解码器必须是全函数。`Linked` 缺 `source_path` 只可能来自坏数据（`media/import.rs` 每个链接导入都写路径），把它塞进 `Placeholder` 是撒谎——`Placeholder` 的含义是"restore 会来救它"，而一个丢了路径的链接没有 restore 能救。命名出来比 `Option` 强：调用方现在必须面对它。

**一个中途被实测推翻的假设**。原计划把 `source_path` 从 `AssetFacts` 里删掉、由行编码器写回 JSON 键。两件事让它不成立：① `AssetFacts` 有 `unknown` 兜底，删字段不会删键，只会让它以无类型条目的形式**被读回来**——等于从另一个方向造出要消灭的第二份副本；② 那个键**从来不只是链接位置**：`media/import.rs:440` 给每个导入都写它（含复制进库的 stored），注释自己写着"记住文件从哪来：文件夹面板按它分组"。所以字段留在 `AssetFacts`（收 `pub(crate)`），另给一个具名写入口 `Asset::set_provenance`；`location()` 只在 `Linked` 下读那个键，`set_location` 不清它，两件事不再互相覆盖。schema 的生成列 `source_path` 与它的专用索引因此一格没动。

**顺带删掉的一份重复**：app 里有 5 处各自实现"这条记录的文件在哪"（preview / 检查器字体 / 右键两处 / 网格 model / 反向搜图）。它们全改成调已经存在的 `media::thumb::blob_path`。一条规则写五遍就是会漂的规则——这也是 P1 关门之后才能这样收口的那类事。

**两处行为差异，都只在"没有写者会产生的行"上**，写进 commit 而不埋进 diff：`relink_asset` 原先拒绝一条"linked 但带 rel_path"的行，现在接受（它的报错文案一直说它要的是 linked 资产）；`purge_assets` 原先会因为一条 linked 行上恰好有 `rel_path` 就去删那个文件，现在只删 `Stored` 指名的——永久删除这条路上，缩小权限是唯一不会说错的方向。

**字节兼容怎么证的**：P2a 那四条测试是**先写、对着旧形状跑绿、再动类型**（键集 24 个、四种状态各把路径写在哪、一条全字段 round-trip、`location`/`set_location` 互逆），所以"导出格式没动"这句话有东西在守，不是我的断言。

**门**：**810 core + 74 app = 884** 全绿 3 ignored；`fmt`、`clippy -D warnings` 同轮过。`crates/` **228 文件 / 115,969 行**，schema 仍 v22。

**没做的**：占位资产的可见行为一字未改（批准范围内）——它在网格里仍表现为"没有文件"，"内容未导入"这句要单独一轮；`set_location(Unrecorded)` 会清掉 `source_path`，这是唯一会清出处的情形，因为那条记录本来就无处可去；P3（`Placement` 收掉 `trashed_at`/`is_trashed`/`in_trash` 三种写法）、P4（`Rating`/`ContentHash` + SQL CHECK + 坏行降级不失败整份列表）、P5（智能集规则 `Json` → `SmartNode`）未动。

---

## 2026-09-28 第十一轮：P3 —— 一条记录在不在回收站，只有一种说法

**落了什么**。三样东西各改掉一种"同一件事写好几种写法"：

① **`Placement { Live | Trashed(DateTime<Utc>) }`** 取代 `Asset::trashed_at: Option<_>`（列也收成 private，入口是 `placement()` / `set_placement()`）。同一个事实在模型里原本是 `Option<DateTime>`，在查询里是 `is_trashed: bool`，在浏览会话里是 `in_trash: bool`——三次拼写，三次都能各自漂。时间戳留着，因为它是事实本身：restore 要清掉它，purge 要报告"回收站里那批是何时进去的"。

② **`TrashPool { Live | Trashed }`** 取代 `AssetQuery::is_trashed`，并且 **`AssetQuery` 不再有 `Default`**——入口只有 `live()` 和 `trashed()`。理由不是好看：`..Default::default()` 会静默填上"活的那池"，一条差一个字段没写的回收站列表于是变成库列表，排序对、分页对、计数对，只有答案是错的。`BrowseContext` 那边同步换成 `pool`，`build_where` 从两个分支各写一遍字面量改成引用常量。

③ **`Flip<T> { id, before, after }`** 取代 undo 里三个批量操作（`SetTrashed` / `SetFavorite` / `SetTitles`）的 `before: Vec<(Uuid, T)>` + `after: Vec<(Uuid, T)>`。两个平行向量拦不住的是同一件事：往一个 push、忘了另一个，撤销于是去改一条正向从没碰过的行，而且**不报错**——错的 id 集、错的长度、错的方向都不会被类型发现。现在一个资产的两个面绑在同一条记录上，`inverse` 对整批只做一件事：逐条交换两面。

**谓词收成两个常量，并且和索引钉在一起**。`LIVE_ROWS` / `TRASHED_ROWS` 替掉 **22 处**生产代码里手写的 `trashed_at IS NULL` / `IS NOT NULL`（assets 7、visual_search 5、stats 4、view_history 2、embeddings 2、smart 规则 1、sequences 1 = 22，`grep` 数出来的，不是估的），外加 plan 测试里那 5 处也改成同一个常量，一共 27 处。真正的理由是：**库里那 6 条 partial index 的定义文本就是这个字符串**，而 SQLite 只会为"能证明蕴含索引谓词"的 `WHERE` 使用 partial index——同义改写（`COALESCE(trashed_at,'')=''`、小写 `is null`、`+trashed_at IS NULL`）答案一样、索引全丢，表现出来是"库变慢了"，不是报错。

两条钉测试，一条看行为一条看文本：**plan 测试的查询现在用常量拼**，把常量改成 `+trashed_at IS NULL` 之后它给出的就是 planner 的原话 `SCAN assets USING INDEX idx_assets_created; USE TEMP B-TREE FOR LAST TERM OF ORDER BY`——正是那 188 ms 的计划；另一条 `the_live_predicate_matches_the_index_it_needs` 从 `sqlite_master` 读出每条 partial index 的存储 SQL，逐条比对常量，并按名字和条数核对那 6 条，所以改 DDL 或漏一条索引同样是红的。**迁移脚本里的字面量故意不动**：一段 step 必须描述它真实产出的形状，v17→v18 那五条索引当年写成什么样就得是什么样。

**撤销那半有实测**：新增的测试把三条资产一起恢复，其中两条本来就在回收站（`before: true`），一条本来活着（`before: false`）；撤销要求各回各的侧。把 `Flip::swapped` 改成不交换，这条红，另外两条原有 round-trip 测试也红——所以它守的是配对，不是我的说法。

**顺带抓到一处文档失真，按代码为准改掉文档**：`sort_desc` 的字段注释写着"`true` (default) = descending"，而它 derive 出来的默认一直是 `false`。本轮**保留代码行为**（升序）并把注释改成实话：面向用户的列表全都自己指定方向（`BrowseContext`、CLI 的 `--asc`、分析任务），所以这个默认只是"没问过的人拿到的东西"。要不要把类型默认改成最新在前，是 P4 的决定，不该藏在一次类型重构里。

**门**：**812 core + 74 app = 886** 全绿 3 ignored；`fmt`、`clippy -D warnings` 同轮过。`crates/` **228 文件 / 116,283 行**，schema 仍 v22，`en.toml` 仍 **798** 键（本轮没加文案）。

**没做的**：`store/assets.rs` 里三处**注释**仍写 `trashed_at IS NULL` 字面量——那是解释文字不是谓词，改它只会让解释更难读；P4（`Rating` / `ContentHash` 两个 newtype + SQL `CHECK` + 坏行降级而不失败整份列表）、P5（智能集规则的裸 `Json` → `SmartNode`，并决定 `Library::evaluate_smart_collection` 那颗零调用方去留）未动；静默丢弃返回值的 `let _ =` 也仍是原样。口径别混：第七轮数的是**守着一次写**的那些（51 处，P0 处理了 11 处，剩 ~40，其中 17 处是 `config.save()`）；本轮重新数的是**全部** `let _ =`——`grep` 实跑 **app 114 处 / core 121 处**，这里面有多少吞掉的是一次真会失败的写，要逐条读才能定，本轮没有逐条读。

---

## 2026-09-28 第十二轮：P4 —— 星级是五个之一，坏行只坏它自己那一行

**四个决定先记下**（都问过了，都选了推荐项）：`Rating` 只装 `1..=5`，未评仍是 `None`；DB 那半用**守卫触发器**而不是列 `CHECK`；读不到的行**跳过 + 一行 `tracing::warn!`**，不加 UI；`ContentHash` **不在这一片**。

**类型这一半**。`Rating(u8)` 内部只允许 1..=5，`new()` 对越界回答 `None`，`clamp()` 是给"外面来的数"用的（模型的答案、XMP 包）。范围规则原先写在**三处**：`AssetPatch::validate`（唯一会被绕到的那处）、`ai/analysis.rs:452` 的 `filter(|&r| (1..=5).contains(&r))`、搜索框 `parse_rating` 的同一个区间。三处都删了，规则只剩类型本身。

**顺手实测出的一个真错**：模型答 `261` 以前会变成**五星**——`n.as_u64() as u8` 把它回绕成 5，而后面那个 `1..=5` 过滤器看到的是 5，合法。现在换 `u8::try_from`，回绕不了。测试单独一条（`a_wild_rating_number_does_not_wrap_into_the_top_star`），因为它错得很像有把握。

**我在 commit message 里写错了一句，这里更正**：我说旧代码"`"rating": "seven"` 能过而 `"rating": "4"` 没人解析"——**不成立**。旧解析器两个都处理了（字符串走 `s.trim().parse::<u8>()`），`"seven"` 一直是 `None`。逐条比过旧新两版：`4`/`"4"`/`0`/`7`/`-3`/`"seven"` 结果**完全相同**，唯一行为差异就是那个回绕。过说的部分撤回，实测的部分留下。

**DB 那一半**：`assets_rating_insert_guard` + `assets_rating_update_guard`，越界值 `RAISE(ABORT, …)`。没用列 `CHECK` 的原因是代价：SQLite 不能给已有列加 `CHECK`，那等于重建 `assets`——**21 条索引 + 3 条挂在 assets 上的 outbox 触发器**全得重挂，而这段迁移的工作本来是不碰行。v22→v23 因此是纯增量 DDL，外加一步数据折叠：老 bound 允许的 `0` 折成 `NULL`（`0` 是"没评"，不是"评了零"，星级条画不出来），**>5 故意不折**——那不是本 build 写得出的值，折它等于猜用户的意思，读侧按"未评"处理即可。

**写这一步顺手挖出迁移器一个从没被触发过的 bug**：`apply_upgrade` 把 step 的 SQL 按每个 `;` 切开逐条执行，而触发器体是 `BEGIN SELECT …; END;`——所以**第一个带触发器的迁移**就会把半条语句交给 SQLite，报 `incomplete input`，而且此后**每一步**都开不了库。切分器现在认得 `BEGIN…END`，测试同时钉住玩具例子和真实的 v23 步骤。六条迁移测试在修好之前全红，这是它真的会发生的证据。

**读侧降级**：`rows::query_map_skipping_unreadable` —— 一行解码失败就丢掉并 `warn!` 出行身份与原因，其余照常返回，末尾再报一条丢弃总数。换成了 4 个多行读：`by_ids`、`query` 的取页、视觉签名回扫、签名补齐。单行 `get` **保持报错**，因为点名要一条资产的人没有"其余的行"可退，而"读不到"和"不存在"是两个答案。`rating` 越界走另一条路：那一格降级成未评，**行留着**——一个列里的坏数不能说明这条记录不是一文件。

**"坏行"到底能怎么坏，是测出来的**：写测试时我先用 `origin` 造，被列 `CHECK` 挡了（那条 CHECK 从第一个形状就在）；改用 `extra` 塞坏 JSON，SQLite 直接报 `malformed JSON`——因为生成列 `source_path` 对它跑 `json_extract`，**写不进去**。真正没被约束的只有 `kind`（一个词表）和 `rating`（这轮起了守卫）。所以降级覆盖的是 `kind` 这类，而"整条 JSON 烂掉"这一类今天仍会让 SQL 语句本身失败，不是解码器失败——这条限制写进了测试注释，别让它看起来已经解决。

**顺带发现、没解决**：智能集合的规则编辑器把 `0 ★` 当成可选项（`dialogs/rules.rs:849` 的 `(0..=5u8)`）。值本身是规则的比较数，问"0 星及以下"不是疯话；但折叠之后**没有任何行能满足它**，这一项成了死的选项。规则的裸 `Json` → `SmartNode` 是 P5，边界该在那里收，这轮只把这件事写进注释。

**门**：**821 core + 74 app = 895** 全绿 3 ignored（比上一轮 +9：类型 3 条、迁移与守卫与降级读 5 条、1 条替换掉规则搬进类型后没意义的旧测试）；`fmt`、`clippy -D warnings` 同轮过。`crates/` **229 文件 / 116,973 行**（新增 `model/rating.rs`），schema **v23**，`en.toml` 仍 **798** 键。

**没做的**：`ContentHash` newtype（下一片）；`extra` 整列 JSON 坏掉仍会让整条 SQL 失败（要 `json_valid` 之类的写法保护生成列的读取，本轮没动，也没测过代价）；`sort_desc` 的类型默认（第十一轮留下的那个问题）；P5 未动。


---

## 2026-09-28 第十三轮：两处"没说出口的失败"——设置写盘，和 0×0 被说成 1×1

排序沿用第七轮定下的那条：**数据完整性排在功能前面**，而这一轮做的是那张表里剩下的两颗 S。两处都是"程序知道自己没做成，用户不知道"。

**① 设置写盘失败，以前没有任何人知道。** `grep` 数出来的是 **23 处**：17 处直接 `config.save()`，6 处是内部会 save 的 mutator（`toggle_filter_tool` / `forget_library` / `record_update_check` / `skip_version` / 加減监听文件夹两个）。它们全写成 `let _ =`，于是一次满盘或权限变化产生的序列是最坏的那种：开关照.flip、程序照当成功、**下一次重启值就弹回去**，中间没有任何一句话。

现在的形状：23 处都走 `app::settings_write::note(result, what)` → 翻转一个进程级 `AtomicBool` + **首次失败打一条带原因的 `warn!`**（后续同类降级成 `debug!`，否则一个会话反复保存会把日志刷满）→ 状态栏挂一条常驻警告"设置无法保存：重启后这些改动会丢失"，tooltip 说还能做什么。这条形状是**照 P0 给任务日志做的那一套抄的**，不是新发明的模式。

两个决定是问过你之后定的：**用状态栏而不是即时 toast**——因为 `plugins/builtin.rs:116` 和开库时的更新检查这两个写者根本拿不到窗口，为了弹 toast 得把错误一路往上传签名；**不回滚内存里的值**——用户刚点的开关当场弹回去看起来像 app 抽风，而那个值在本次会话里确实是生效的，诚实的说法是"可能活不过重启"，不是"这事没发生"。旗标跟的是**最后一次**写的结果而不是锁死：盘清出来之后再次保存成功，警告就消失（测试里专门钉了这条不锁死）。

**② `probe.rs` 把 0×0 的 RAW 头报成 1×1。** `.max(1)` 的错不在"数字小了"，在于**它把"不知道"变成了"知道，而且知道错了"**：未知是可以补的，而一条 1×1 会进分辨率筛选、进宽高比筛选、画出一张卡，且后续没有任何一环知道它曾经是个零。现在返回 `None`，与同一个文件里 `video_facts` 对 mp4 轨道"没有像素就没有尺寸"的做法一致，而库里本来就有 5 条记录带着"尺寸未知"活得好好的。顺手把**负数**也拒了——`as u32` 会把 `-1` 变成一个巨大的尺寸，那是同一类谎言的另一个方向。

**这颗没有夸大**：实测过它**今天造不出脏行**——导入流水线对 RAW 扩展名**故意不做表头读取**（`dimensions_need_full_decode` 把它推给解码阶段），而解码阶段会用真实像素覆盖尺寸；这台机器上那份真库 `width=1 OR height=1` 是 **0 行**。所以修的是"这个函数下次被谁调到时说什么"，测试的注释里也这么写，不写成救了已有数据。

**门**：**822 core + 75 app = 897** 全绿 3 ignored；`fmt`、`clippy -D warnings` 同轮过；`statusbar.settings_not_saved` 与它的 tooltip 键**九个目录都加了**（`i18n.rs:104` 那条棘轮测试仍然过，它守的是"七个小目录不能比英文再多缺键"）。`crates/` **230 文件 / 117,167 行**，`en.toml` **800** 键（+2），schema 仍 v23。

**没做的**：这 23 处以外的 `let _ =`（第七轮数的"守着一次写"的 51 处里剩下的那些，以及 `library.rs:151` 建根合集时 `let _ = collections::set_appearance(...)` 这种**写库**的静默失败）；状态栏警告的**观感**我在这里验不了——它要真的写盘失败才会出现，我只有逻辑层的测试（`settings_write.rs` 里那条翻转/清除的测试是这一轮唯一的机器验证，23 处调用点的接线没有测试覆盖）。


---

## 2026-09-28 第十四轮：用户做的改动没写进库，以前一句话都不说

**18 处**（`grep` 数出来的，不是估的）：app 里向库发起"用户刚做的动作"、然后把返回的 `Result` 用 `let _ =` 丢掉的地方 —— 标签面板 6（拖选打标、两处标签颜色、删除、创建、改名）、资源管理器 4（两处重命名、拖进回收站、拖进合集）、检查器 3（标签输入框三条路径）、选择工具条 3（还原、收藏、加入合集）、素材右键 1、`remember_model_look` 1。

**为什么会专门伤到用户**：这些动作的界面反馈全部来自**"被要求的状态"**而不是"被存下的状态"。标签芯片出现、合集名字改了、素材离开回收站——写没成功都一样，于是下一次重启把它放回去。中间没有任何一句话。这是第七轮那张表里"数据完整性排在功能前面"的最后一颗 S。

**形状**：17 处走 `LibraryController::report_failed(what, outcome)` —— 日志留下原因，状态栏那条 notice 说"这次改动没有写入素材库：{error}"（新键 `notice.change_not_saved`，九个目录都加）。**并且每个调用点照旧 bump `generation`**：重读会把库里真实的样子摆回屏幕，所以那颗芯片会自己消失，同时原因在屏幕上。这两件事必须成对——只报 notice 会让屏幕继续和文件不一致，只默默刷新则看起来像"我点的东西没反应"。

**第 18 处故意不给 notice**，并且把理由写在代码里：`remember_model_look` 是 3D 模型光照变化时的记录，面板早就把光照画在屏幕上了，为了"没存下这个视角"打断一次拖拽比丢掉它更糟。那处只记日志。**"这里我们选了沉默"必须带理由**，否则下一个读代码的人会当成又一处漏掉的。

**顺手多修了一层**：检查器那串逗号输入的吞异常其实有**两层**（`ensure_tag` 和后面的 `tag_assets`），只包外层等于留一半。现在第一个失败就停止报（notice 只有一行）但继续走完剩下的名字。

**验证边界说清楚**：17 处接线**没有测试覆盖**——它们是点击处理器，要真的 `LibraryController`。有测试的是共用形状的骨架（`settings_write.rs` 那条翻转/清除）和新键带来的目录对齐（`app/i18n.rs:104` 那条棘轮）。真要在屏幕上看到这句话，得让一次写真的失败。

**本轮实测到的下一颗（没动）**：`library_manager.rs:737-738` 在"删除库"这条确认路径上把两次 `std::fs::remove_dir_all(库目录 / 缓存目录)` 吞了，而它前一行**已经把配置里那条记录删掉并落盘**。失败时的状态是：入口没了、数据还在盘上、用户以为删干净了——一个找不到的目录比一个删不掉的目录更难发现。同一族，量级 S，只是这次它不属于"用户动作写库"那一类，所以另开一片做。

**门**：**822 core + 75 app = 897** 全绿 3 ignored；`fmt`、`clippy -D warnings` 同轮过。`crates/` **230 文件 / 117,247 行**，`en.toml` **801** 键，schema 仍 v23。


---

## 2026-09-28 第十五轮：删库的两步换了顺序，以及一次我自己造成的返工

**上一轮末发现的那颗**。`app/library_manager.rs` 里"删除这个库"是两步：**先** `forget_library`（一次配置写盘，成功即落），**后** 两次 `std::fs::remove_dir_all`（库目录 / 缓存目录），而后两次的结果被 `let _ =` 丢掉。失败时停在最坏的那一格：**库文件还在盘上，指向它的入口已经没了，屏幕上一个字都没有**——一个找不到的目录比一个删不掉的目录更难发现。

**顺序换过来了**：先删文件，两个目录真的没了才动入口。中间态仍然有两种，但两种都点名说清楚：

| 中间态 | 现在做什么 |
|---|---|
| 目录被拒绝，数据库**还在** | 入口**保留**（行还能打开、还能从同一个菜单重试），toast 说出是哪个路径、为什么 |
| 走到一半才失败，数据库**已被删** | `remove_dir_all` 是边走边删，所以这格真的可能出现；此时列着它也打不开了，于是**入口删掉**，toast 说明盘上还剩什么 |

第二种是**检查出来的，不是推的**：`!data.join("library.db").exists()`。这一步决定了入口该留还是该走，猜错的方向正好是这轮要消灭的那种状态。

**一个必须写下来的边角**：`clear_directories` 把"目录本来就不存在"算作删除成功，不算失败。因为一个建好但从未打开过的库**没有缓存目录**，`remove_dir_all` 会报 `NotFound`——如果那算失败，就会给一个**真被删掉了的库**留下入口。三条测试钉的就是这一格、"停下来并点名拒绝它的那个路径"、以及"两个都删干净时什么都不报"。用**普通文件**当那个"拒绝"（`remove_dir_all` 不会拆一个文件），这样测试不依赖权限、不依赖磁盘忙不忙、也不依赖谁在跑它。

**toast 是从哪来的**：库管理器**没有** `LibraryController` 可以挂 notice，所以它用这轮之前就已经在用的那条路——`dialogs/convert.rs` 和导出备份那处的 `window.push_notification(Notification::warning(…))`。`on_ok` 的第二个参数本来就是 `&mut Window`，只是以前没接。三个新键（`delete_incomplete` / `delete_failed` / `entry_left_behind`）九个目录都补齐。

**这一轮我自己砸了一次，说清楚**：做变异验证时我用了 `git checkout -- app/library_manager.rs` 想撤销实验改动，而那文件上有**这一轮还没提交的全部改动**——于是 helper、`on_ok` 重写和三条测试一起被丢掉。`~/.qoder-cn/file-history` 里查不到这个文件的快照（`grep -rl clear_directories` 零命中），所以是照本会话里我写过的同一段编辑**重打一遍**再验的：三条测试重跑绿，并且**重新逐条确认它们在规则被抽掉时会红**（`.exists()` 守卫抽掉 → "absent counts as removed" 红；`return Some(...)` 抽掉 → "walk stops and names" 红）。第二次变异改用 `cp` 到 /tmp 再 `cp` 回来，不再碰 git。这是同一类错误的第三次（前两次是把 stash 当读工具、把注释当事实），规则本身已经写进记忆：**实验前先把文件复制到 /tmp，撤销用 `cp`，永远不要用 `git checkout -- <path>` 当"恢复"**，工作树里有未提交的活。

**门**：**78 app + 822 core = 900** 全绿 3 ignored；`fmt`、`clippy -D warnings` 同轮过。`crates/` **230 文件 / 117,414 行**，`en.toml` **804** 键，schema 仍 v23。

**没做的**：删除路径仍不是事务——`remove_dir_all` 走到一半失败之后，被删掉的那部分不可能靠这轮的任何顺序救回来；真要治好它得先改名隔离再删、并在下次启动时清扫残留（那是本轮提问里的第三个选项，你选了第一个）。另外 `app` 里剩下的 `let _ =` 已经不是"用户动作的结果"那一类（`update`/`send`/`recv`/`open_url`/`fs` 清理等），要不要继续收是另一件事。


---

## A. 自动化与可扩展性（最大一块）

Trove 现状：`Plugin` trait 编译进二进制（`trove-core/src/plugins.rs:56-75`，配套 app 侧 `AppPlugin`：`trove-app/src/plugins/mod.rs:38-52`），唯一参考实现是 `builtin::SidecarNotes`，且它在 `plugins/mod.rs:69-80` 里是**硬编码**的一个；对机器开放的口只有 `trove` CLI 的 **20 个顶级 / 26 个叶**命令（枚举体 `trove-cli/src/cli.rs:59`，另有 `:428 CollectionCommand` 6 条与 `:473 IndexCommand` 2 条；上一版记的"28 叶"偏高两个）和采集服务的 `GET /ping|/health`、`POST /add|/fetch`（`services/collect.rs:6-13`、路由 `:311-417`）——后者另有一个浏览器扩展做投递端（仓库根 `extension/`），但它只喂导入，不是控制面。Serpent 现状：一个 **87 条命令**的自动化网关（`src/automation/command-registry.ts:1723-3529`，按 `commandId:` 逐条数得；上一版本这份文档写的"约 91"偏高），其中 **86 条以 MCP 工具暴露**——唯一不暴露的是 `ui.widget-patch`，它 `allowedSources: ['plugin']`（`:2165`）。工具表不是手写的，是从那份注册表**投影**出来的（`src/mcp/tool-catalog.ts:141` `listSerpentMcpTools`，全文 220 行），网关本体 `src/main/embedded-mcp-server.ts` 894 行。外加 QuickJS 脚本沙箱和完整插件 SDK。

| 差距项 | Serpent 参照 | Trove 缺什么 | 建议落点 | 量级 |
|---|---|---|---|---|
| MCP 服务器 | `src/main/embedded-mcp-server.ts`、`src/mcp/tool-catalog.ts` | 完全没有：全仓 `mcp\|json-?rpc\|tool.?catalog\|tools/list` 只命中这份文档本身。Agent 只能通过 CLI 间接驱动 | 复用 `trove-cli` 已有的命令树：把 `cli.rs` 的命令 + Zod 式输入校验抽成一份声明，同时喂 clap 和 MCP tools/list | L |
| 危险操作两步确认 | Challenge：`planHash` + `idempotencyKey` + `acknowledged`，改参数/换客户端/重放/过期一律拒。字段的注入点在 `src/mcp/tool-catalog.ts:117-145`（`withChallengeConfirmationFields`，`acknowledged` 是**常量 true**，所以模型无法自行"确认"），机制本体 `src/mcp/mcp-challenge.ts` 99 行 + `src/main/mcp-operation-challenge.ts`；`criticalOperation: true` 只挂在 **2** 条命令上（`command-registry.ts:1895`、`:2319`），闸门 `tool-catalog.ts:72-77` 对着 7 项的 `automationCriticalOperationRegistry`（`command-registry.ts:381`），其中仅 2 项 `exposedToMcp` | 只有字符串开关：`purge` 缺 `--yes` 直接拒（`trove-cli/src/write.rs:396-400`，开关声明在 `cli.rs:135`）。`idempoten\|plan_hash\|acknowledg` 在 `crates/` 下**零命中**；UI 侧的确认是 gpui 对话框（`app/library_manager.rs:711`、`panels/workspace/toolbar/title.rs:322`），不跨进程 | 先把"幂等键 + 计划哈希"建成 core 里的一个类型，CLI 和 MCP 共用 | M |
| 权限/能力目录 | **48** 项权限枚举（`src/plugins/plugin-manifest.ts:170-219`）→ **25** 项 capability（`src/automation/command-registry.ts:112-140`，逐条带 `defaultPolicy`），`riskTier: 'safe'\|'controlled'\|'critical'`（`:148`）与 `defaultPolicy: 'allow'\|'ask'`（`:150`）确实都在。本行上一版记的 47→22 两头都偏低 | 无。任何调用方都等于全权——唯一长得像的东西是 HTTP 403 的 `VendorErrorKind::Permission`（`ai/vendor.rs:81`）和一个 shader 测试里的 naga `Capabilities`（`preview/gpu3d.rs:1143`） | 从 `auto` / `full-access` 两档起步即可，不必一次做 48 项 | M |
| 执行日志与对账 | `src/main/automation-execution-journal.ts`（**51,151 B / 1,254 行**，"51 KB"这个说法核实无误）+ `execution.status` | 客户端超时＝未知结果，没有对账口。库里的表一共 **13** 张（第十二轮之后重数过，行号也是当天的），全在 `store/schema.rs` 的 `SCHEMA`：`assets:448` `collections:547` `asset_collection:562` `tags:573` `asset_tag:581` `smart_collections:594` `view_history:613` `model_looks:624` `asset_sequences:632` `asset_sequence_frames:640` `search_queue:655` `asset_embeddings:697` `task_journal:720`——原先列的第 14 张 `ai_analysis` 已经在 v22 被删（第九轮），只有 v14→v15 那一步还创建它——**没有一张记"谁调了什么、结果如何"**。新增的 `task_journal` 记的是**本应用自己的后台任务**（谁起了、跑到哪、成功还是失败、重试了几次、起止时间戳），不是"哪个调用方发起了一次自动化请求"：它没有 caller / principal 这一列，CLI 与未来的 MCP 也都不经它。**Serpent 那张 journal 的用途是"客户端超时后仍然能回答这次到底成没成"，这一格仍完全没做** | 一张 `automation_journal` 表 + 一个 `trove execution status <id>`。`task_journal` 是它现成的邻居，形状可照抄 | M |
| 脚本沙箱 | QuickJS UtilityProcess：CPU 10s / 墙上 60s / 内存 64MiB / 输出 1MiB / ≤4 并发 / ≤128 pending promise —— **六个数逐个核过，全对**（`DEFAULT_AUTOMATION_EXECUTION_RESOURCE_BUDGET`，`src/main/automation-execution-journal.ts:108-115`；UtilityProcess 侧 `src/scripting/script-runtime-utility*.ts`，映射在 `src/main/automation-script-ipc.ts:351-356`）；能力授予绑定「脚本哈希 × 目标库 × 能力集」（`automation-execution-journal.ts:636-643`，匹配器 `:1097-1112`，检查点 `:592`） | 完全没有，且**没有任何可借的引擎**：`rquickjs\|deno\|boa_engine\|quickjs\|v8\|wasmtime\|lua` 在 `Cargo.lock` 里零命中 | `rquickjs` + 只暴露领域 API；预算照抄上面这组数，它们是被打磨过的 | L |
| 插件动态加载 | manifest **1,003 行** Zod 已核实（`src/plugins/plugin-manifest.ts`）。贡献点严格说是 **14** 个键：13 类带数量上限的（commands ≤256、menus 每桶 ≤256、toolbar ≤64、inspector ≤64、viewerActions ≤64、shortcuts ≤64、views ≤128、dialogs ≤32 可选、settings ≤128、hooks ≤128、jobs ≤128、providers ≤128、themes ≤8）**再加一个可选的 `ui`**（`pluginContributesSchema` `:755-773`，strictObject）——本表上一版漏了 `ui` | 编译期两个 trait、**5 类贡献点**：core `Plugin` = `name` / `pipeline_stages` / `commands`（`trove-core/src/plugins.rs:56`、`:61`、`:75`），app `AppPlugin` = `translations` / `settings_pages` / `run_command`（`trove-app/src/plugins/mod.rs:38`、`:45`、`:52`）。没有目录扫描也没有清单解析：`init` 把唯一插件**硬编码**成 `builtin::SidecarNotes`（`plugins/mod.rs:69-80`），语言包走 `include_str!`（`plugins/builtin.rs:157-158`）。代码自己也这么写：`plugins.rs:20-22` "compiled in for now … dynamic loading are the next steps" | 第一步只做「清单文件 + 从目录加载」，贡献点先留 commands / settings_pages / pipeline_stages 三类 | XL |
| 插件任务与租约 | 持久表名其实是 **`jobs`**（v27 的 `jobs_v27`，`library-service.ts:3024`）——本表上一版写的 `plugin_jobs` 在 Serpent 里不存在。`recovery_strategy IN ('idempotent','checkpoint')` + `attempt_count`，checkpoint 存在 `payload_json` 里（`src/worker/plugin-job-repository.ts:61`、`:115`、`:482-512`），逐项定向重试已核实 | 11 种内置 `TaskKind` + **`TaskKind::Custom`**（`tasks/mod.rs:92-111`；`Custom` 携带插件给的名字，`Cow<'static, str>`，所以 `TaskKind` 已不再是 `Copy`）。**"注册表只是内存态、重启不续"这句本轮改了半句**：`store/task_journal.rs` + schema **v21** 的 `task_journal` 表，每次状态迁移写一行（start / 终态 / retry 各自一条，`retry_count` 累加），`library.rs:320-328` 开库时挂一条独立连接进去（WAL 下与主连接共存），自定义 kind 以 `plugin:<name>` 前缀存所以能原样读回。**读侧已于 2026-09-28 第二轮接上**：`Library::assemble` 在开库时、把连接交给 `TaskManager` 之前调 `load_interrupted()`，读到的行由 `Library::interrupted_tasks()` 交给状态栏任务面板顶部一块独立的"上次退出时中断"区。**不提供"重试"是有意的**：journal 记的是什么任务、跑到哪，从不记启动它的输入（`Retryable` 只活在本次会话的 controller 里），所以面板能说"你的导入停在 1200 里的 320"，不能替用户决定再跑一次。`RetryPolicy` / `start_with_retry`（每重试从一个 `factory` 取新闭包、带 backoff、发 `TaskEvent::Retrying`）与 `start_with_priority` 同样在第二轮拿到了真实调用方（嵌入回填 / AI 分析 / 其 undo / 导入 / 模型解析），`Plugin::task_kinds()` 也被 `assemble` 喂给了 `TaskManager::declare_task_kinds`，并且**这道声明有牙**——没声明过的 `TaskKind::Custom` 现在被 `StartError::UndeclaredKind` 拒掉。真正跑着的两件：所有任务改共用一个 `TaskPool`（`tasks/mod.rs:527`，`max(2)` 个 `trove-pool-N` 线程，以前每任务一条），以及进度事件按任务合流成最新一条（`MAX_EVENTS_PER_JOB = 128` 从此只约束终态事件）。所谓 "checkpoint" 仍只是协作式暂停/取消点（`:14-16`），库里另一个重启后还在的后台状态仍是 `search_queue` 那张触发器喂的出站表 | 门面、journal 往返、读侧、重试与优先级都已接上（2026-09-28 第二轮）。剩的是 `recovery_strategy` 那一层：Serpent 区分"幂等重跑"与"从断点续跑"，Trove 的重跑**总是从头再来**（靠作业本身的幂等性兜住，见 §2026-09-28 第二轮 的表），而协作式 checkpoint 仍只是暂停/取消点 | M → **已收**（除 `recovery_strategy` 语义，未估） |
| 域事件 / 钩子 | 事件 2 种（`library.changed` / `asset.changed`，`pluginDomainEventKindSchema` 在 `src/plugins/plugin-domain-events.ts:10-13`），可阻塞钩子恰好 1 个：`pluginHookEventSchema = z.enum(['asset.trash'])`（`src/plugins/plugin-hooks.ts:13`），`PLUGIN_HOOK_DEFAULT_TIMEOUT_MS = 2_000`（`:9`），超时与返回畸形都 **fail-open**（`:45-47`，注释解释了为什么 `:66-68`） | 无进程内事件总线：`EventBus\|emit\|subscribe` 在 core 与 cli 下零命中，只有 gpui 的逐实体事件（`panels/mod.rs:41` `EventEmitter<PanelEvent>`、`workspace/interactions.rs:296`、`:339`），也不存在可否决的前置钩子——插件阶段只能**追加在挖掘之后**（`plugins.rs:115-121`） | Trove 已有 `search_queue` 触发器这套"变更外发"底子，接一个进程内广播不难 | S |
| 派生字段 | provider **8** 种已核实（`src/plugins/plugin-manifest.ts:582-591`：preview / thumbnail / metadata / import / export / ai / derived-field / search），派生字段落表并参与搜索与筛选 | 无通用机制。最接近的是 `ai_analysis` 按 `(asset_id, model_version)` 落库（`schema.rs:312`）+ `asset_embeddings`（`:559`），两者都可重算，但字段名是写死的、不参与 `AssetQuery` | 有插件系统之前不必做 | — |
| 拖出到系统 | Windows 真文件剪贴板在 `src/main/win32-file-clipboard.ts`（135 行）：koffi 绑 `user32.dll` / `kernel32.dll`，预定义格式 `CF_HDROP = 15`（`:17`、`:42-43`），消费方 `src/main/file-clipboard.ts`；另有原生拖拽出 | ◐ **本行上一版记错了，而且是这一轮最值得记的一种错**：按 Serpent 的实现符号去 grep（`CF_HDROP\|xdnd\|file_drop` 确实零命中）就记成"没有"，而没有按**能力**问一句"文件能不能拖出窗口"。**能力有**，自 `ce6a29d`（2026-09-12）起：网格格 `panels/workspace/cells.rs:199-215`、列表行 `:494-510` 各注册一次 `external_drag_payload`，产出 `gpui_kit::ExternalDragPayload::Files(FileDragPaths::new([(path, false)]))`，注释就写着"promote the in-app drag to a native file drag handed to the OS"、以及"必须注册在 `on_drag` 之后、同一个负载类型"。交出去的是**真文件**：`library.rs:437-444 asset_file` 对 linked 给 `facts.source_path` 原件、对 stored 给 blob，文件不在则 `None`。**平台支持度在 gpui 那一侧，不在这里**：trait 默认 `can_start_external_drag() = false` / `start_external_drag() = false`（`gpui-pre-0.3.5/src/platform.rs:1005`、`:1008`），只有 **Wayland**（`gpui-pre-linux-0.3.5/src/linux/wayland/window.rs:2011`、`:2015` → `client.rs:578-615`，offer `text/uri-list`，常量 `wayland/clipboard.rs:22`）和 **macOS**（`gpui-pre-macos-0.3.5/src/window.rs:2218`、`:2222`）覆盖了它；X11 与 Windows 后端对这个符号**零命中**，即那两个平台上拖出就是 `false`。此外出得去的另两条老路不变：交给外部应用打开、或在文件管理器里 reveal（`services/open_external.rs:75`、`panels/common.rs:176`）。**仍然缺的三件**：① 只拖出指针下那**一个**资产——闭包捕获 `id` 而把 `AssetsDrag` 负载丢在参数里（`_: &AssetsDrag`），与同一次 in-app 拖动携带"整个选中集"不一致；② 没有文件**列表剪贴板**（`arboard` 那处只写图片 `library/clipboard.rs:65-83`，文本走 `panels/inspector.rs:655`），所以"Ctrl+C 一个资产、到文件管理器 Ctrl+V"这条 Serpent 用 `CF_HDROP` 走通的路在这里不通；③ 上游没有 X11/Windows 实现。另可对照：in-app 负载是私有类型 `AssetsDrag`/`CollectionDrag`/`SmartDrag`，`ExternalPaths` 作为**入站**拖放在 `app/root.rs:978` | ①是多选闭包改吃 `payload.0` 的 ids、`FileDragPaths` 本就吃数组——**S**；②要 `arboard` 之外的路（Linux 上把同一份 `text/uri-list` 写进 clipboard 的 `CLIPBOARD` 数据源，macOS 要 `NSFilenamesPboardType`）——**M**；③只能等上游或本地补一层 | S + M |

> **判断**：这一块的差距不在"功能点数量"，在于 Trove 缺一个**形式化的命令契约**。`cli.rs` 里其实已经有 20 条命令和一份严格契约（stdout 恒为一个 JSON、退出码 0/1/2/3、诊断走 stderr），把它结构化就是 MCP 网关和脚本 API 的地基——这是 Trove 相对 Serpent 的**后发优势**，Serpent 是先在 CLI 上摔过一次（CLI 已撤回）才建网关的。

---

## B. 同步与外部库

| 差距项 | Serpent 参照 | Trove 现状 | 量级 |
|---|---|---|---|
| WebDAV 双向同步 | `src/worker/sync/` 共 **2,240 行 / 9 个文件**（行数逐行核过）：`SyncEngine`（`sync-engine.ts:112`）→ `RemoteStorageDriver`（`remote-storage.ts:51`）→ `WebDAVDriver`（`webdav-driver.ts:194`），Basic + **Digest 自动协商**（`webdav-driver.ts:240-246`、`:408`）、`If-Match` CAS 写（`:225`）、PROPFIND/MOVE/MKCOL 自动补父链（`:6-7`）、207 解析（`:289`、`:383`）、能力探测（`:389-496`）、自签 TLS 开关 `allowInsecureTls`（`:132`、`:236`）；密码进系统钥匙串走 Electron `safeStorage`（`src/main/index.ts:1081`、`:1154`、`:4073`）；事件 5s 去抖（`src/main/sync-auto-scheduler.ts:84-86`）+ 轮询比对 manifest | 没有 DAV 这一个协议：`webdav\|PROPFIND` 在 `crates/` 下零命中，每一个 `sync` 命中都是 `std::sync` 或注释（例 `store/tags.rs:257` 的 "re-synced"）。**但上一版把这句写成"唯一的对外网络口子是入站采集"是错的**——出站 HTTP 客户端早就在图里并且在用：`ureq`（`Cargo.lock:9034`，注意**不是** `reqwest`，后者零命中）见于四个 vendor 适配器（`ai/vendor/openai.rs:220`、`anthropic.rs:142`、`gemini.rs:136`、`dashscope.rs:131`）、`ai/embedding_openai.rs:96`、更新检查（`services/update.rs:135`、`:143`）与采集的 URL 拉取（`services/collect.rs:735`）。采集服务本身是**入站**、且只监听本机（`services/collect.rs:207` 的 `TcpListener`）。所以真差距只剩"DAV 协议层 + 冲突决策表 + 跨设备身份"，**不是**"要先引入一个 HTTP 栈" | L |
| 冲突模型 | 纯决策表 `src/worker/sync/sync-plan.ts`（文件头 `:1-13` 就是这张表）：单边改→传/下；双边改且哈希不同→ **LWW 定胜负**（`conflictWinner` `:83`），败方留 `名字 (conflict-YYYYMMDD-HHMM).ext` 副本（落盘在 `sync-runner.ts:99-107`），**绝不静默覆盖**；本地删→ `delete-remote` + `tombstone-upload`（`:60-61`）；「行在、文件不在」判为**下载**而非墓碑——源码里就写着"禁止当成删除写墓碑"（`:11`，条件 `localMissing` `:74-78`，这条曾是 bug）；元数据走独立侧车通道 `.serpent-sync`（`sync-metadata.ts:2`、`manifest.ts:26`），**SQLite 永不上传**。六条全部核实 | 无 | 含在上条 |
| 跨设备身份 | `assets.sync_id`（迁移 v38），本地 `asset_id` 保持设备私有 | 无。`assets` 的 **25** 列（`schema.rs:328-371`，含 v20 新增的 `source_path` 虚拟列）里没有任何跨设备身份：`id` 是本地 UUID 主键（`:329`），唯一能在两份拷贝之间对上的是 `content_hash`（`:342`），而它对"同内容两个资产"和"同资产两设备副本"是不区分的 | 同步开工前必须先定这一条，事后加要迁移 | 
| 「打开云端库」 | `sync.list-remote-libraries` / `sync.open-remote-library`（`src/main/worker-client.ts:105-106`，处理器 `src/main/index.ts:3670`、`:3681`，UI `src/renderer/OpenSyncLibraryDialog.tsx`），靠 manifest 识别、续同一库身份。**注意这两条是 Worker 的 IPC 请求类型，不是自动化命令**，所以没算进上面那 86 个 MCP 工具 | 无 | M |
| Eagle 库导入 | `src/worker/eagle-library.ts` **533 行**（行数核实）：`info/*.json` + `images/`，`folders`→合集（`:403`）、`annotation`→描述（`:399`）、**`star`**→评分（`:400`。本表上一版写的是 `rating`，那是它导出侧的字段名，源 JSON 里叫 star）、`url`→`sourcePageUrl`（`:401`），**复用 Eagle 已有预览图**免重编码（`:36-37` 的注释明写"视频拿它当海报，不再编码一次"），导入后写 `asset_auto_analysis_suppression`（v37 表，`library-service.ts:3435`）**永久抑制自动 AI**（Eagle 路径 `:42158`） | 无任何 DAM 迁移入口 | M |
| Billfish 库导入 | `src/worker/billfish-library.ts` **651 行**（行数核实）：直接读它内部 SQLite（只读 + `fileMustExist`），目录层级→托管文件夹；同样写抑制表（`:42421`） | 同上 | M |
| 归档入口 | **12 种**后缀：`.zip .eaglepack .rar .7z .tar .gz .tgz .bz2 .tbz .tbz2 .xz .txz`（`src/main/external-library-archive.ts:44-56`；`.billfishpack` 只在流式集合里，`:59`）——本表上一版只列了 6 种。libarchive-wasm（`:14`、`:239-243`）、空间预检（`:152`、`:173`）、孤儿清扫（`sweepOrphanExternalLibraryStaging` `:433`）。**但"可取消"不成立**：这个文件里没有 `AbortSignal` / cancel 任何一条路径 | 归档会被**分类并当成一个资产导入**：`.zip .rar .7z .tar .gz .bz2 .xz .iso` → `AssetKind::Archive` + mime（`media/probe.rs:109`、`:167-170`），目录会递归展开（`tasks/import.rs:158`、`:236`）。但**不解包**：`zip` crate 只开了写侧 `features = ["deflate"]`（`trove-core/Cargo.toml`），`ZipArchive` 仅出现在测试里（`services/archive.rs:362`、`:386`、`:468`），双击归档是交给外部应用打开（`panels/workspace/open_with_apps.rs:58`） | M |
| 备份可还原 | — | 写侧齐、读侧为零：`services/archive.rs` 只有 `create_full_backup:66` / `write_full_backup:74` / `build_archive:84` / `write_manifest:162`，两个保存对话框调它（`app/library_manager.rs:266-273`、`app/root.rs:620-627`），头注释 `:10-12` 把还原写成"手工解压覆盖"。`services/backup.rs` 的每日 `VACUUM INTO` 快照在设置页列得出来（`dialogs/settings/files.rs:386-413`），**同样没有 restore 函数**。别被 CLI 那个 `Restore` 命令骗了——它是**从回收站恢复资产**（`cli.rs:129-130` → `write::restore`），不是还原备份 | S |

> **注**：Trove 的导入是"只链接"，同步必须先回答"链接文件怎么同步"——Serpent 的答案是**链接文件夹内容不参与同步**，只同步托管内容。这条约束值得直接抄下来。

---

## C. 内容类型覆盖

| 差距项 | Serpent 参照 | Trove 现状 | 量级 |
|---|---|---|---|
| **序列帧** | 完整系统：自动检测（仅显式导入后提示，`src/renderer/post-import-image-sequences.ts`，一次最多提示 64 组）、手动创建确认窗（`image-sequence-import-dialog.ts` 的 `purpose: "import" \| "create"`）、Inspector 播放（`image-sequence-playback.ts`）、全选覆盖未加载项（`image-sequence-selection.ts`，`LIST_ASSETS_BY_ID_MAX = 10_000`）、**隐藏帧不单独入队**（靠 `asset_sequence_frames.position > 0` 过滤，`library-service.ts:15930-15932`、`:16195`、`:16269`）。共享判据在 `src/shared/image-sequence.ts:8-9` | ◐ **数据层已落地（2026-09-24，T7 的一半），09-25 复核：用户能碰的那一半一行都没有**。已有的：v19 两张侧表（DDL 在 `store/schema.rs:135`、`:145`）+ `media/sequence.rs:115 detect` 分组纯函数（判据与它逐条对齐：两式编号、零间隙、≥3、补宽分区）+ `store/sequences.rs` 的 `create:70` / `dissolve:202` / `set_fps:216` / `membership:233` / `hidden_beside:37` + 隐藏帧那一句在 `build_where` 与四个计数面。**那四道 grep 是 09-25 的证据，2026-09-28 第二轮逐条重跑，四条全部翻过来了**：`trove-cli/` 现在有 `sequence` 一组三个子命令（`create --fps` / `dissolve` / `fps`）、菜单文案的键有了（九份各 4 条 `workspace.*sequence*`）、门面方法五颗（`create_sequence` / `dissolve_sequences` / `dissolve_for_assets` / `set_sequence_fps` / `sequence_of`），`trove-app/` 侧右键菜单两颗接上它们。**还缺的是会动的部分**：`media/sequence::detect` 这个分组纯函数仍然没有任何调用方（导入后逐组提示没有）、卡片轮播没有、序列播放器没有，所以逐帧文件成组之后仍然只是"少了一堆卡"，看不到它在放。默认帧率新加了 `media/sequence::DEFAULT_FPS = 24.0`（菜单用它，CLI 可给 `--fps`） | **L → 手动那半已收（S），播放那半仍 L**（理由见 §内容类型计划末尾） |
| 文本 / 代码查看器 | **43** 个扩展（`src/shared/text-media.ts:5-48`，上一版写 44，偏一）；载入上限 **1 MiB** = `TEXT_VIEWER_MAX_BYTES`（`:108`），"经有上限的 Worker IPC 载入"（`TextViewerControls.tsx:35-36`）；行号槽 `:141`、`:327`；保存**返回新 `revisionId`**（`src/preload/index.ts:1631-1642`）。⚠️ 上一版说"换行开关持久化"**不成立**：那只是模块级 `let persistTextViewerWrap`（`:39`），会话内有效，不落盘 | ✅ **已做（2026-09-24，T3/T4）**：`components/preview/text.rs` 用 gpui-kit 的 `Editor`（`readonly`、行号槽、虚拟行、自带选择与复制）——**Serpent 那一格反倒是手搓的一个 `<textarea>` 加一列 `<pre>` 数字，既不高亮也不虚拟**。清单 `media/text.rs::is_text_ext` **96** 个后缀（比它的 43 个更广：`.wgsl` `.glsl` `.srt` `.xmp` `.env` `.editorconfig` 这些都在内），1 MiB 上限，**截断写在头部而不是藏起来**。折行开关同样是会话内的（与它一致）。**只读**：没有写回，所以 Serpent 那个"截断缓冲被存回源文件"的 bug（`save()` 只看 `editable` 不看 `truncated`）在这里不可能发生；真要写回需要 CAS，而 Trove 没有 `revisions` 表，比较对象只能是 mtime + size。文本卡片见下一行 | ✅ 已平（写回另说） |
| **编码探测** | BOM → UTF-16 NUL 奇偶 → UTF-8 致命解码 → ICU `chardet` 统计（`src/shared/text-encoding.ts:182-231`），`DetectedTextEncoding` 恰好 **9** 个取值、含 gb18030 / big5 / shift_jis / euc-kr（`:15-24`）；NUL >2% 判二进制（`:215`）。另一条常被归错地方的洞见：**刻意不从字体 cmap 猜界面语言**（194/1171 个含假名的本地字体实际是中/韩）——它写在 `src/shared/font-metadata.ts:14` 与 `:445`，属于字体元数据那一层，不是文本查看器 | ✅ **已做（2026-09-24，T3）**：`media/text.rs::decode` 就是那五级，判据逐条对齐它（BOM → NUL 奇偶 → **NUL >2% 判二进制** → 严格 UTF-8 → `chardetng` 统计），头部报出实际用的编码。用的是 **`chardetng 1.0.0`（ICU 那个探测器的 Rust 本体，Serpent 的 JS `chardet` 是它的另一层绑定）+ `encoding_rs`**（后者本就在 `Cargo.lock:2002`）。两处它没有的：**UTF-32 的 BOM 被显式排除**（不排就会当 UTF-16 解出一串像模像样的假中文，测试钉着），以及**截断点落在半个字符上时先裁尾巴再判 UTF-8**（否则一份正常的 UTF-8 文件会被误报成 legacy 编码——它有专门的测试）。文本内容**还没进全文索引**——09-25 把"没进"钉成了证据（`grep -n "media::text\|read_text\|text::" search.rs` 零命中，所以 `media/text.rs` 那套解码器与索引之间**没有任何一条线**），**这条证据 09-28 重跑仍然成立**；变的是理由：当时写的是"那要 `INDEX_VERSION` 升版"，而版本号从 2 一路涨到了 5，**升版早已不再是障碍**，缺的只是把正文喂进去的那一个字段 | ✅ 已平（索引待办，且不再需要批准） |
| 文档真缩略图 | PDF 首页在 Worker 里用 `pdfjs-dist` + `@napi-rs/canvas` 渲染（`library-service.ts:22781`、`:22820`），**落成一条普通缩略图记录**（卡片/悬停/检查器零特例）；HTML 用一次性沙箱 offscreen 窗口 `capturePage()`（`src/main/document-thumbnail-renderer.ts:4-8`、`:51`、`:83`） | ◐ **文本那一半已做（2026-09-24，T4）**：`thumb::ensure` / `regenerate` 的 `_ => None` 臂现在先问一句 `is_text_ext`，文本资产画的是**它自己的前 11 行**（`write_text_card` → 生成 SVG → 复用 `render_svg_data`，所以系统字体带 fallback，中文不是豆腐块）。走 SVG 那条路而不是自己排字，是因为那是本 crate 唯一一处"解析字体家族并会 fallback"的地方。卡片仍按内容哈希寻址，所以**没有新的缓存目录要登记**（`clean_orphans` / `storage.rs` 都不用改），但 `plan_thumbnail_rebuild` 的 kind 表补了 `Document` / `Other` 并按后缀过滤，否则重建会漏掉它们。**PDF / 归档仍是 kind 图标**：2026-09-24 的决定是宁可这半行留在差距表里也不加依赖（路线已比过：外部 `pdftoppm`→`mutool`→`gs` 链零依赖，随包 pdfium 换保真度但要塞每平台几 MB 共享库）。HTML 那半**改道**：`.html .htm .xml .css .js` 本质是文本，查看器给的是真内容真卡片，比一张 1024×800 截图有用，也免掉一整套沙箱与"故意不设截止"的机器 | PDF 那半仍开（M） |
| EXR / HDR 图像 | OIIO 解 EXR（`ocio=studio-v4-aces2`，`library-service.ts:27242`）+ **曝光 ±10 档**（`Math.max(-10, Math.min(10, …))`，`:27151`）+ plane/part 选择（`subimage`，`:27242`、`src/shared/library-api.ts:151`） | ✅ **已解并可见（2026-09-24，T2）**：`image` 开 `exr`/`hdr`/`tga` 三特性（`exr` 1.74.2 本就在 `Cargo.lock:2170`，从未接线；`hdr`/`tga` 在 image 内零依赖），`probe.rs` 补后缀与 mime（`image/aces`、`image/vnd.radiance`、`image/x-tga`），新建 `media/hdr.rs`：曝光 ×2^stops → ACES 胶片拟合（Narkowicz）→ sRGB → 8 位，alpha 不参与。**09-25 复核：滑杆这一格比上轮记的便宜，因为"核"已经做完了**——`tonemap(image, stops)` 本来就是吃参数的（`hdr.rs:43`），±10 的范围连同"合成软件为什么给十档"的理由都写在 `hdr.rs:19-23`，而且**已经在内部钳位**（`stops` 越界不外溢），非零档有测试钉着（`:198-199` 比 -2/+2 两档的明暗，`:211` 钉"8/16 位普通图原样返回"）。缺的只有调用方：两个调用点都传死 `0.0`（`hdr.rs:33 open_for_display`、`thumb.rs:506`），`trove-app` 里对 `tonemap`/`MIN_STOPS`/`MAX_STOPS` **零命中**。所以它现在是一处纯 UI 工作，代价问题见 §内容类型计划末尾的"明确不追"。**plane/part 选择器仍然没有**（Serpent 自己也把它藏进 `length > 1` 才出现的 `<select>`）。顺带两处：TGA 无 magic bytes，`image::open` 按后缀能解但**头信息尺寸**读不到，`probe::image_dimensions` 为此加了指名格式的一臂；EXR/HDR/TGA 在像素编辑里**就地只读**（8 位重编码等于把 HDR 换成它的降级副本），工具条说明原因而不是给一颗注定失败的按钮 | 滑杆 **S**（核已就绪）· plane/part M |
| **色彩管理** | ACES 工作室配置写死为 `SERPENT_OCIO_CONFIG = 'ocio://studio-config-v4.0.0_aces-v2.0_ocio-v2.5'`（`library-service.ts:446`），**8** 个可选手动输入空间（`src/shared/image-color-space.ts:10-19`）+ 逐资产覆盖表 `asset_color_space_overrides`（v22，`library-service.ts:1989`），**不改源文件**（`:1986` 原文 "the source remains untouched"） | 无：`icc\|icm\|ocio\|aces` 按词边界扫 `crates/` **零命中**。像素编辑是**主动丢弃**：`media/edit.rs:9-12` "Re-encoding drops ancillary metadata (EXIF orientation, colour profiles)"。EXIF 方向只在解码时应用一次（`media/convert.rs:208-213`、`thumb.rs:739-744`）。`media/color.rs` 名字像但做的是主导色量化，不是色彩科学 | L |
| 音频资产 | 一等：卡片封面=内嵌专辑图，否则用波形当封面。**两个尺寸别再混**：卡片是 **640×480（≈4:3）**，而 **1280×220 是查看器 / Inspector 里那条波形带**，`src/shared/audio-media.ts:20-31` 自己就写明了两者不同（上一版把 1280×220 记成了卡片尺寸）。悬停即播、波形时间线（`audio-waveform-timeline.ts`）。另有 `audio_proxy`，但它是**源解码失败时的兜底**（`library-service.ts:28406`、`:31122`），不是常规转码管线 | **已经可听**（P3/P4/P5，2026-09-23/24）：预览分派按 `kind == AudioKind::Audio` 起 `AudioPlayer`（`components/preview/audio.rs`，分派点 `preview/mod.rs:372`），控件与视频共用 `preview/transport.rs`（9 档速度 0.25×–4×）；卡片优先内嵌封面、否则用包络画一张 512×288 波形卡（`media/thumb.rs:607-650`）；预览上方一条波形带（`media/waveform.rs`，400 桶）。**2026-09-24 那条带能拖了**（T1：按下/拖动移动播放头、松手才 seek，与滑块共用同一个 `scrub_to`，拖动时在带上画一条播放头）。~~T5 之后只剩一条：`artist`/`album` 不进全文索引~~ **那一格已于 2026-09-27 收掉**：`INDEX_VERSION` 走到 3 时 `artist_words` / `album_words`（各配一路 `_tri`）就进了解析表，检查器与检索看的是同一份数据，`artist:` / `album:` 两个限定词在 `expression.rs:260-269`。即播已随 §C 的活卡片那一行一起做掉（同一条 `LiveCard` 通道，音频卡起播、在卡上移动即跳；2026-09-25 起改为空格触发）。**09-28 音频侧再多一格**：采样率/声道/位深/比特率有了自己的索引面（`audio_words` / `audio_tri`）与五个限定词。**音频这一节至此没有待办** | ✅ 已平 |
| AI 分析覆盖面 | 图像 + **视频**（`library-service.ts:26620-26673`：`fps=1/interval` 抽帧、`drawtext` 打 `pts:hms` 时间戳、`tile=CxR` 拼一张，行列数夹在 4…20 以保证 ≥16 帧，`keyframeCount >= 16` 时走关键帧快路）+ **3D**（离屏四视图接触表 `:22611`） | ⚠️ **三类都接了，但视频那类只接了一半**：`media_type_of` 把 Image / Video / Model 映射到 `MediaType::{Image, Video, Model3D}`（`tasks/ai_analysis.rs:568-575`）；3D 走离屏转盘渲染卡（`media/thumb.rs:364` + `media/render3d.rs`），这条是平的。**视频这条本行上一版记错了**：它说接触表"导入期算好"并列出 `ai_analysis.rs:844-856` 为证据——那个函数（`contact_sheet_path`）**只做 `path.is_file().then_some(path)`，全仓没有任何代码写这张表**，所以 `request.contact_sheet_jpeg` 对视频永远是 `None`，视频实际只发了一张缩略图。四个 vendor 适配器搬运的代码都在（`ai/vendor/{openai,anthropic,gemini,dashscope}.rs`），缺的是生产者。**生产者已于 2026-09-24 补上（T6）**：`media/video.rs::write_contact_sheet` 一次 ffmpeg 出 16 格带时间戳的 JPEG，`ai_analysis` 首次用到时现做并按内容哈希缓存，目录也登记进了清理与容量报告。取向的差别还在（Serpent 分析时现抽、Trove 现做后缓存一份），但**这一格现在是平的** | ✅ 已平 |
| 卡片悬停 seek | clientX → 0..1 时间轴；位移 ≥6px 判 seek（定帧 + 播放头），静 500ms 翻转为播放——两个常量都在 `src/renderer/asset-card-hover-scrub.ts:7`、`:10`（`CARD_HOVER_SCRUB_MOVE_THRESHOLD_PX` / `CARD_HOVER_SCRUB_SETTLE_MS`），接入点 `AssetCardMedia.tsx:90`。**同时只 1 张卡能 scrub**（`use-asset-card-hover-preview.ts:46-67` 一份 `hoveredState`，`App.tsx:13843` 只把 preview 给那张卡） | ✅ **已做（2026-09-24，T5）**：`components/preview/quick_look.rs`（原名 `hover.rs`）的 `LiveCard` 实体——起初 500ms 定住才起、**同时只 1 张**、指针一离立刻停。视频走 `FramePipe`（**320 宽**，不是播放器那个 720：一张卡不该占播放预算），mailbox + 60ms presenter 让解码环永不借 `App`；卡内移动 ≥2% 宽度算 seek，**画面与声音一起 `restart_at`**（只跳画面会整段错开）。音频同一条通道起播，顺手关掉 §C 音频那行的第三条。**两处与 Serpent 不同，都是有意的**：它是"移动定帧、静止翻转成播放"两个状态，我们是一个状态里跟着指针走（gpui 每次 seek 要重启 ffmpeg 进程，两态来回会抖）；它把 GIF 也算进可悬停，我们不动图片卡（图片本来就有动图在播）。**开关**：~~`settings.hover_media`~~。**2026-09-25 触发方式改了**：不再由指针定住 500ms 起，改成选中卡片按**空格**（`QuickLook` 动作）。绑在 `AssetGrid` 而不是 `Workspace` 上下文——搜索框也在 `Workspace` 里，绑在那儿会把空格从打字里吃掉；开关随之删除（它不会自己动、也不会自己出声，没有需要关的东西）。文件改名 `quick_look.rs`，实体改名 `LiveCard`；图片与字体也并进这条通道（右侧浮层放大，`deferred` + `Positioner`） | ✅ 已平 |
| 视频截当前帧入库 | — | ✅ **已做（2026-09-24，T1）**：`media/video.rs::write_frame_png(path, at_ms, out)` 一次 ffmpeg 出**源尺寸**的 PNG（走 `proc::slot()` + 超时，临时文件改名落位，失败不留半个文件），预览工具栏一颗相机按钮（只在有活播放器时出现，`has_video`），文件写进 `paths::incoming_dir()` 后按**链接**导入——与截图入库同一条链。**没有**取屏幕上那块 `shown` 缓冲：播放器把解码宽度限制在 `DEFAULT_MAX_WIDTH = 720`，一张要留下的图不该继承播放预算。原先那两处文档失真已随实现一起改成实话 | ✅ 已平 |
| AI 查询改写的 UI 入口 | Serpent **有代码没入口**（2026-09-24 再确认）：planner 在 `src/main/ai-search-planner.ts`，IPC 挂在 `src/main/index.ts:3109`，preload 暴露 `planAiSearch`（`src/preload/index.ts:1511`），而 `src/renderer` 下**没有任何调用点**——唯一沾边的是 `App.tsx:15352` 一个类型映射辅助函数 | Trove **三级 L1/L2/L3 已接通**，2026-09-24 复核调用链完整：`library/jobs/search.rs:140-144` 真调 `ai::search_planner::plan(provider, asked, cancel)`，产出的 `AiSearchPlan` 存进控制器状态（`library/controller.rs:328`），再回到 `search.rs:544`、`:573`、`:592`、`build_plan_query` `:872` 参与排名；分级开关是 `SearchTiers`（`browse.rs`）由 `resolved_search_tiers` 从配置解析（`controller.rs:363-364`） | —（Trove 领先，列出只为防止误判成要追的洞） |

---

## D. 检索纵深

| 差距项 | Serpent 参照 | Trove 现状 | 量级 |
|---|---|---|---|
| ~~**分页会话**~~ | Worker 侧有序快照 `src/worker/browse-session-store.ts`（文件头 `:60` 自称"活跃浏览快照的有界 LRU"），会话 LRU **32** 核实：构造函数的默认值在 `:69`，实例化点 `library-service.ts:6712`；按 `libraryGeneration` + 持久 `changeSequence` 失效（两个字段 `:23-24`）并返回类型化 `stale`；`declaredTotal` 另取 COUNT | ✅ **已做（2026-09-24）**：`BrowseContext::snapshot()` 产出一个 `BrowseSession`——**排名型视图（搜索 / 最近查看）把整条有序 id 列表冻在会话里**，翻页只是切片；集合型视图（普通浏览 / 智能合集）仍由 SQL 开窗，因为为一个 200 行的页去收集十万条 id 比这一页本身更贵。总数在冻结时数一次（`assets::count` / `smart::count`），不再是每页一次 COUNT。网格因此删掉了"按 id 去重"那层兜底：同一份快照里不可能有重复。会话随视图身份变化被丢弃，不引入 LRU——桌面端同一时刻只有一个可见列表 | — |
| ~~**结果上限**~~ | 无硬上限：首页可以只是已绘制窗口 | ✅ **已彻底收敛（2026-09-24，覆盖 09-23 那条）**：`CANDIDATE_CAP = 2000` 只剩"无过滤条件时的快路"；一旦有筛选要拒绝行，池子直接取该词项的**全部**命中，`WIDE_CANDIDATE_CAP = 20_000` 那一档**已删除**，换成 `MAX_RANKED_POOL = 200_000` 这个只在超大库才可能撞到的天花板（撞到才 `Page.truncated`，标题栏"至少 N 项"、CLI JSON 的 `truncated` 字段就是为它留的）。09-23 那轮的实测（3751 项库里那颗 2000 池之外的星标资产）如今不再依赖"加宽"这一步 | — |
| ~~**窗口上限 1000**~~ | — | ✅ **本轮发现并拆掉（2026-09-23）**：`store/assets.rs` 与 `store/smart.rs` 各有一句 `limit.min(1_000)` 静默钳制（来自 `635b3f0` 那次初始重构，无注释无文档）。因为网格用"把 limit 越问越大"来分页，超过 1000 项的库**看不到第 1001 项**，而标题栏总数是真的，于是触底永远 `loaded < total`、每次滚动都重跑整条查询拿回同样 1000 行。现在改成 `MAX_PAGE = 20_000` 的显式拒绝（`checked_limit`），CLI 的 `--limit` 用同一常量在校验层报用法错，`--offset 3600` 实测取回 151 行 | — |
| ~~智能合集的总数~~ | — | ✅ **已对齐（2026-09-23）**：智能合集分支以前把方向 / 宽高比 / 最低星级 / `ext:` 和输入框 qualifier 留在 SQL 取完一页**之后**于内存里过滤，于是 COUNT 数的是没过滤的集合、列表显示的是过滤后的——头部数字虚高、页短于页长、分页走向一个到不了的数。现在这些谓词经 `assets::where_fragment` 下推进 smart 的 `WHERE`，窗口与总数看同一批行（`browse.rs::smart_grid_filters`）。合集 / 标签 / 文件夹**仍不参与**，这是有意保留的语义：智能合集本身就是容器 | — |
| ~~布局虚拟化~~ | >5,000 资产时身份/几何按 100 行摘要页随滚动补，页面 LRU 24，槽位按索引稳定故不重挂 | ✅ **渲染侧与追加分页都收敛了（2026-09-24）**：`Row` 的两个列表换成 `Rc<[Cell]>` / `Rc<[f32]>`，追加一页走 `rows::appended_rows`——头行按引用计数存活，只有新窗口被布局，每窗成本从 **O(已加载)** 变成 **O(新页)**。判据是渲染里那个 `appended` 标志：只有 `run_page_pass` 那条分支才是"同一条列表尾巴变长"，导入 / 改名 / 删除走的是替换路径，照旧整体重排。`refill_rows` / `timeline_rows` / `materialize_rows` 全改成借 `&[Cell]`，克隆只发生在真正放进那一行的时候（时间线的日分组改为排**索引向量**，不再排序 cells 本身）。顺带：每帧的 `row.cells.clone()` / `row.widths.clone()` 也变成引用计数。仍留一件：时间线视图每次重排，因为它的分组确实跟着集合动 | — |
| ~~覆盖索引~~ | 迁移 **v40 / v41 / v42** 就是为分页建的偏索引（`library-service.ts:3562`、`:3567`、`:3572`）：v40 `ASSETS_ACTIVE_NAME_INDEX`、v41 `ASSETS_ACTIVE_CREATED_DESC_INDEX`、v42 `TOMBSTONE_BACKFILL_INDEX`。前两条正是"过滤 + 排序"的复合偏索引，理由是冷页读取在 SMB/NAS 上就是网络往返 | ✅ **索引这半已做（2026-09-24，schema v17→v18）**，并且**测出了真正的病根不在索引**：100k 合成库、200 行一页、offset 40k 上量过——默认浏览 **188 ms**（计划是 `idx_assets_trashed` 驱动 + `USE TEMP B-TREE FOR ORDER BY`，即每页把全库排一遍），加了偏索引但**没有统计**仍然 188 ms，`PRAGMA optimize` 也仍是 188 ms（它抽样 2000 行，把 `trashed_at` 报成"每值 2001 行"的选择性，而真值是 100000 行全为 NULL），**只有全量 `ANALYZE` + 偏索引 = 1.9 ms**。所以 v18 这一步装的是两样东西：五条 `(排序列 DESC, id ASC) WHERE trashed_at IS NULL` 偏索引（`created_at` / `file_name COLLATE NOCASE` / `size_bytes` / `rating` / `(kind, created_at)`），以及一次 `ANALYZE`；`Store::ensure_statistics` 在每次开库时按"`sqlite_stat1` 里记的行数 vs 现在的行数，漂移超过 1/5 就重跑"守着，不新增任何状态位。`id` 必须是第二列：分页用 `…, id ASC` 破同秒并列，索引供不上这个破法就照样落 temp B-tree。**只建五条**：`updated_at` / `duration_ms` / 主色这三个 `AssetSort` 变体在排序菜单和 `trove --sort` 里都到不了，为它们各花 5–7 MB / 10 万行只剩写放大。测试 `store/mod.rs:a_live_page_is_read_in_index_order` 直接断言计划（含"没有 TEMP B-TREE"）。**顺手修掉一个静默错**：`AssetSort::Color` 的 SQL 表达式写的是 `json_extract(extra,'$.visual.dominant_color')`，而 `extra` 是**扁平** JSON（键就叫 `dominant_color`），所以按颜色排序一直在排一列 NULL，实际顺序来自 `id ASC` 破位 | M（索引半已做，`rank_intersect` 半见下） |
| 查询表达式 | `tag:` `desc:` `author:` `path:` + 别名，空格 AND / `\|` OR / `-` 排除 / `"短语"`，跨 IPC 走 DNF 结构化查询，搜索框内 `?` 弹出语法表 | ✅ **已做（2026-09-23，09-27 扩到相机/艺术家/专辑/字体，09-28 扩到音频并把短语变成真的）**：`crates/trove-core/src/search/expression.rs`，桌面搜索框与 `trove search` 共用一套语义。字段限定词 `name` `filename` `file` `title` `desc` `description` `tag` `tags`（文本面）+ `camera` `make` `model`（相机 EXIF）`artist`（媒体标签艺术家）`album`（专辑名）`font` `family`（字体族）+ **09-28 新增** `audio` `sample_rate` `channels` `bit_depth` `bitrate`（音频规格，`Target::Audio`）；过滤词 `ext\|format` `kind\|type` `path\|folder` `rating\|stars` `fav\|favorite`。无限定词时搜索同时覆盖名字/标题/描述/标签/**复合 facts**（相机·艺术家·专辑·字体·音频规格·嵌入标题）。**带引号且含空格的短语 09-28 起按位置匹配**（jieba 分词后喂 `PhraseQuery`，词序颠倒不答应；只分出一个词时退回普通检索，n-gram 仍并进去所以部分匹配没丢）——以前引号只影响切词不影响词序，这一格名义上有、实际上和没写一样。**差异两条：没有 `author:`**（Trove 无作者字段，全仓零出现）；**语法表没做进 UI**——而且注意 `search_syntax` 那个 locale 键不是帮助文案，它是**降级提示**（"Search continued, ignoring part of the query: %…"，`locales/en.toml:113`，用在 `panels/search_box.rs:59`）。解析不了的片段推给状态栏，**2026-09-23 起一次报全部**，以前只报第一条、其余静默丢弃 | — |
| ~~搜索范围~~ | 严格限定在当前可浏览集内（文件夹 ±递归 / 合集 ±递归 / 智能合集 / 回收站）；文件夹范围下文本搜索默认递归 | ✅ **假阴性已根治（2026-09-24，R2a）**：次序仍然是"先排名后筛选"（`build_where` 的注释原样保留），但**池子不再截断**——有过滤条件时 `pool_for` 一次取回该词项的**全部**命中（`gather_cap(num_docs)`），所以"排名第 2001 位且同时满足筛选的资产"这种情况不再可能存在；`WIDE_CANDIDATE_CAP = 20_000` 那一档被删掉了，换成 `MAX_RANKED_POOL = 200_000` 的**天花板**，只有库超过天花板时 `Page.truncated` 才为真（标题栏"至少 N 项"那条路径保留，语义变准确了）。代价实测（100k 文档索引，debug 构建）：2000 id 99 ms / 20000 id 297 ms / 全量 100000 id 1.02 s，**线性、每次冻结付一次**（`BrowseSession`），不是每页。测试 `a_filtered_search_finds_the_match_that_ranked_past_the_fast_path`（2005 篇文档，弱命中那颗排在最后并且是唯一收藏）+ `a_filtered_gather_is_capped_only_above_the_ceiling`。**纵深索引已走到 `INDEX_VERSION` 5**（09-27 到 3 覆盖元数据 facts，09-28 的 4/5 见 §D 本节第 13 条），但 R2b 的"把可过滤列镜像进 Tantivy"仍没做——现在只剩性能意义（fast field 才能压 stored-doc 读），假阴性已在 R2a 根治 | — |
| ~~命中高亮~~ | snippet 以 `{assetId, text}` 回传，卡片与检查器高亮，**不改存储** | ✅ **已做（2026-09-24），但没走 snippet 那条路**：索引能说出哪些文档命中，说不出该画哪几个字节——靠 2-gram / 拼音 / 模糊编辑进来的命中在原文里没有对应那一段。所以 `search/highlight.rs::Lexicon` 改问一个能从行本身答上来的问题：用户输入的词（按 `Target` 分面）在这段文本的哪里出现，大小写不敏感、重叠区间合并。列表行用 `gpui::StyledText::with_highlights` 上色（`panels/workspace/cells.rs:marked`），**一个文本布局而不是并排的 span**：名字要截断加省略号，span 各自测量会把省略号画到错处。解析每窗口一次（`cells_for`），用的就是排名所依据的同一套语法，两者不可能对"用户要什么"有不同意见。**两处诚实的空白**：① 拼音 / 模糊命中不标（`mao` 找到 `猫` 是真的，但那里不画线）；② **网格磁贴根本不画文字**（`cells.rs` 的 tile 只有缩略图 / 类型图标 / 字体样张），所以可标的文本面只有列表行；检查器那边是可编辑 `Input`，标不了 | — |
| ~~分辨率分档~~ | 1K (<2240) / 2K (2240–3199) / 4K (≥3200) 三档 chip | ✅ **已做（2026-09-24）**：`ResolutionBand`（`model/asset.rs`，按**长边**分档；边界取 2240 / 3200 两个"没人按这个尺寸出货"的中点，于是不需要宽高比那套 ±3% 容差）+ `AssetQuery.resolution` + `build_where` 里一句 `CASE … ELSE MAX(width, height) END BETWEEN ? AND ?`（无宽高的行取 -1，落在校外，与方向 / 宽高比同一契约）。工具栏是**独立的一颗** `分辨率`：`FILTER_TOOLS` 加了 `resolution`（默认不显示，"+" 菜单开），因为它与那两个形状筛选**正交**——"4K 且 16:9" 是两个条件同时成立，塞进单选的形状菜单会说谎。CLI `--resolution 1k\|2k\|4k`；智能合集经 `smart_grid_filters` 同一条下推，COUNT 与窗口看同一批行。实测（128 项真库）：SQL 直数 1K=117 / 2K=6 / 4K=3 + 2 条无宽高，`trove list --resolution` 三档分别报回 **117 / 6 / 3**；`store/browse.rs:resolution_bands_filter_by_the_longer_edge` 钉住 2240 边界、竖图按高决定、以及形状+分辨率复合 | — |
| ~~颜色筛选相似度~~ | HSL 匹配盒 + **0–100 相似度滑杆**（hueSpan 55°→10°、satΔ 0.5→0.1、lightΔ 0.42→0.1 随相似度插值），编译进 SQL 走 `dominant_hue/saturation/lightness` 偏索引；灰黑中性只按明度匹配，彩度低于地板不算「红」 | ✅ **语义与滑杆已做（2026-09-24）**，SQL 编译那半没做，理由在下面。`media/search.rs` 新增 `Hsl` / `rgb_to_hsl` / `ColourMatch::from_similarity` / `score`：三轴区间就是那三个插值，**最差的一轴定分数**（平均会把"暗红"和"鲜红"算成近似），色相按圆周取短距（350° 与 10° 差 20°）。中性规则是**非对称**的，而且写反一次就是个静默 bug：灰/黑/白**问**（彩度低于地板）只按明度答；灰/黑/白**候选**永远不响应一个有彩度的问——第一版写成了"任一方中性就只看明度"，于是灰色以 1.0 满分混进红色检索，是 store 层那条 `a_colour_search_narrows_as_the_slider_tightens` 逼出来的。滑杆在取色器弹层里、HSLA 四栏之下，0–100、**松手才提交**（与网格缩放同一套路：拖动期间每帧重跑检索就是"还没选完就换了结果"）。`search_by_color` 不再解码直方图，改读记录的 dominant color(s)；**没调色板的老资产（`compute_and_store_signature` 回填的那批）仍退回直方图余弦**，所以先于调色板挖出来的库不会突然查不到。相似度落在 `LibraryController::colour_similarity`（默认 50，不写配置文件：它是这一次检索的问法，不是偏好）。**性能实测（release，50k 张签名图）**：旧路径 **671 ms** → 现在 **175–234 ms**；剩下这笔的下限不是扫描也不是打分，而是**每行 1.1 KB 的直方图文本必须先读进来才能知道它不是答案**——要再往下只有一个办法：把 HSL 三值提成真实列（Serpent 正是如此），而那是一次重写整张 `assets` 的迁移，所以这一轮**没有做**，v19 因此空着留给 §C 的序列帧（后来 v19 确实给了序列帧两张侧表，v20 加了 `source_path` 虚拟列）。`extra LIKE '%visual_phash%'` 那个谓词也一并删了（它是个子串测试，能匹配到 *值* 里的同名串），不是"换成了 SQL 预筛"。**仍然没有**：`dominant_hue/saturation/lightness` 落库、检索结果按分数二次排序的可调阈值、以及网格磁贴上的分数角标（磁贴不画文字，分数目前只在列表视图那一列，见"命中高亮"行同一限制） | M → 已做（除落库列） |
| AI 搜索结果里"文件夹" | 结果区单列 Folders，前端在已加载导航树上匹配，最多 8 条 | 无。搜索的产出是一条扁平 id 列表：`folder` / `collection` / `group` 三个词在 `panels/search_box.rs` 与 `panels/workspace_search.rs` **双双零命中**，而 `workspace_search.rs:1-4` 的注释明写搜索结果"接管工作区网格本身"——搜索结果与浏览共用同一个网格，那里没有第二个区放 Folders。要做，形状是网格上方一条横向容器条，不是新页面 | S |

### 检索纵深计划（2026-09-23 本轮已做 + 剩余顺序）

**本轮已做，全部在 3751 项的合成库上实测过**（`trove list --offset 3600` 取回 151 行；19 个 200 行窗口拼起来与整表逐 id 相等，15 ms/窗且随深度不增长）：

1. 拆掉两处 `limit.min(1_000)` 静默钳制 → `MAX_PAGE = 20_000` 的显式拒绝，CLI `--limit` 同常量报用法错。
2. 网格分页从"把 limit 越问越大"改成"取缺失窗口并追加"，附带按 id 去重与"到底即停"。（这道去重兜底在次日 R1 之后**已经删掉**——快照里不可能有重复，见下表 R1）
3. 智能合集的方向 / 宽高比 / 星级 / `ext:` / 输入框 qualifier 下推进 SQL，COUNT 与窗口看同一批行。
4. 排名池饱和且存在过滤条件时自动加宽到 `WIDE_CANDIDATE_CAP = 20_000`；仍不够就 `Page.truncated`，标题栏说"至少 N 项"、CLI JSON 多一个 `truncated`。实测：一颗确实落在 2000 池之外的星标资产，`probe fav:yes` 现在查得到（改前这条查询只能返回空）。
5. 搜索框一次报出**全部**解析不了的片段，不再只报第一条。

**2026-09-24 第二轮（R7 / R5 / R4，全部在 128 项真库 + 715 条 core 测试 / 66 条 app 测试上跑过）**：

6. **R7**：`Row` 的 cells / widths 换成 `Rc<[Cell]>` / `Rc<[f32]>`，三个行构造器（`refill_rows` / `timeline_rows` / `materialize_rows`）改成借 `&[Cell]`，并新增 `appended_rows` 走"头行复用 + 只布局新尾巴"。判据是渲染里新增的 `appended` 标志——只有 `run_page_pass` 那条分支才是同一条列表变长，替换路径照旧整体重排。时间线的日分组改为排**索引向量**（原先是克隆整个列表再 sort）。测试 `an_appended_page_keeps_the_rows_it_already_had` 用 `Rc::ptr_eq` 钉住"头行是共享的，不是重建的"。
7. **R5**：`ResolutionBand`（按长边，边界 2240 / 3200）+ `AssetQuery.resolution` + `build_where` 的 `MAX(width,height) BETWEEN` CASE + 工具栏独立一颗 `分辨率`（`FILTER_TOOLS` 加 `resolution`，默认关，"+" 菜单开）+ CLI `--resolution 1k|2k|4k` + 9 份 locale（键数一致，i18n 棘轮没动）。实测：同一库 SQL 直数 117 / 6 / 3（+2 条无宽高），CLI 三档分别报回 117 / 6 / 3。
8. **R4**：`search/highlight.rs::Lexicon`——从**已解析**的 `Expression` 取正项 `Atom`（按 `Target` 分面），在渲染文本上求大小写不敏感的字节区间并合并；列表行经 `gpui::StyledText::with_highlights` 上色（一个文本布局，不是并排 span，否则省略号位置会错）。9 条单测钉住：多次出现、CJK 字节偏移、`desc:` 不标名字、`-词` 不标、重叠合并、拼音**不**标（这是有意的空白）。
9. **R3 前半**（schema v17→v18）：五条 `(列 DESC, id ASC) WHERE trashed_at IS NULL` 偏索引 + 一次全量 `ANALYZE` + `Store::ensure_statistics` 的开库漂移检查。**这是本轮量出来的真正病根**：100k 合成库上默认浏览一页（offset 40k，200 行）无统计 **188 ms** → 有统计无偏索引 **57 ms** → 两者齐备 **1.9 ms**；而 `PRAGMA optimize` **不管用**（它抽样，把 `trashed_at` 报成"每值 2001 行"，真值是全库 100000 行 NULL），必须全量 `ANALYZE`。计划断言 `store/mod.rs:a_live_page_is_read_in_index_order`（含"不许出现 TEMP B-TREE"），迁移断言 `a_v17_library_gains_the_ordered_indexes_and_their_statistics`，并在**真库副本**上实跑一次 v17→v18：版本 18、五条索引、18 行 stat1、计划变成 `SCAN assets USING INDEX idx_assets_live_created`。顺带修掉 `AssetSort::Color` 的 `$.visual.dominant_color` 扁平键错配——按颜色排序一直在排一列 NULL，实际顺序来自 `id ASC` 破位。
10. **R3 后半**：`rank_intersect` 的候选 id 从"每个一个 `?`"改成**一个 JSON 数组参数**（`id IN (SELECT value FROM json_each(?))`），语句文本不再随池宽变化，`rows.rs:26/44` 两处 `prepare` 同时换成 `prepare_cached`——这一步才让它有意义。100k 库、20000 候选实测：**旧形状 214 ms**（其中 **137 ms 是 SQLite 解析两万多个占位符本身**，行工作只占小半）、临时表方案 124 ms、定长分块 131 ms、**json_each 54 ms** 且解析成本与 N 无关（0.15 ms）。行为测试 `the_ranked_intersection_keeps_rank_and_drops_gone_rows` 钉住"幸存者按秩返回、已删 id 静默消失"；计划测试改名 `ranked_intersection_never_drives_off_a_filter_index`，并记下一个新事实：**换成这个形状后，连不加 `+` 前缀的 Driving 子句也不再走筛选索引**——所以前缀如今是"保证"而不是"救命"，而统计缺失时（每个新库都是这个状态）它仍是唯一那道保险。
11. **R2a**（用户选的这条路）：有过滤条件时池子不再取固定宽度，而是一次拿回该词项的全部命中；`WIDE_CANDIDATE_CAP` 删除，换成 `MAX_RANKED_POOL = 200_000` 天花板，`truncated` 只在真撞天花板时为真。**假阴性由此根治**（次序没动，但"池子装不下"这个前提没了）。线性成本实测（100k 文档、debug）：2000 → 99 ms，20000 → 297 ms，100000 → 1.02 s，每次冻结付一次。两条测试：`a_filtered_search_finds_the_match_that_ranked_past_the_fast_path`（2005 篇，弱命中那颗确实排在最后）、`a_filtered_gather_is_capped_only_above_the_ceiling`。留 `#[ignore]` 的 `bench_uncapped_gather` 供下轮复测。
12. **R6**：`media/search.rs` 加 `Hsl` / `rgb_to_hsl` / `CHROMA_FLOOR` / `ColourMatch`（`from_similarity(0..=100)` 就是那三条插值；`score` 取**最差轴**、色相按圆周取短距；中性规则**非对称**——灰问只看明度，灰候选绝不响应有彩度的问）。`search_by_color` 改读记录的 dominant color(s)，不再解码直方图；没有调色板的老资产（`compute_and_store_signature` 回填那批）退回直方图余弦，所以老库不会突然查空。取色器弹层加 0–100 滑杆（松手才提交，与网格缩放同一路），值在 `LibraryController::colour_similarity`（默认 50，不落配置文件）。`SimilarAsset.score` 终于上屏：列表视图多一列百分比，只在视觉检索时存在。删掉 `extra LIKE '%visual_phash%'` 谓词。实测（release，50k 签名图）：**671 ms → 175–234 ms**；剩下的下限是"每行 1.1 KB 直方图文本必须先读进来"，只有把 HSL 提成真实列才能再降，而那要重写 `assets` 全表 → **本轮不做**，v19 空着给 §C 的 T7（后来 v19 确实给了序列帧两张侧表，v20 加了 `source_path` 虚拟列）。测试：`a_colour_search_narrows_as_the_slider_tightens`（滑杆收紧是**删答案**不是重排）+ 三条 `ColourMatch` 单测，其中一条就是被这条 store 级测试逼出来的（第一版让灰色以满分混进红色检索）。
13. **纵深索引（2026-09-27 起，2026-09-28 走到第 5 版）**：`INDEX_VERSION` 2→3→**5**（`search.rs:60`，注释逐版记着原因）。3 = Tantivy schema 新增 10 个文本字段（`facts_w/tri`、`camera_w/tri`、`artist_w/tri`、`album_w/tri`、`font_w/tri`），`extract_fact_texts` 从 `AssetFacts` 提取：相机 EXIF（make/model/ISO/aperture/focal/exposure + source_url）、媒体标签（artist/album/embedded_title）、字体（family/style/weight/glyphs）、音频规格（sample_rate/channels/bit_depth/bitrate）；`expression.rs` 新增 `Target` 变体与 7 个 qualifier（`camera`/`make`/`model` → Camera、`artist` → Artist、`album` → Album、`font`/`family` → Font），`gram_query` 与 `term_query_on` 的路由同步。4 = **逐面拼音** `name_pinyin` / `title_pinyin` / `desc_pinyin` / `tags_pinyin`：拼音以前只建在四面拼接的一份索引上，所以 `tag:mao` 这类**带限定词的拼音检索**结构上无法支持（从拼接拼音答上来，命中的可能是文件名的拼音——`expression.rs:40-42` 的模块注释原本把这写成有意取舍，现已改成实话）。5 = `audio_words` / `audio_tri` + `Target::Audio` + 五个限定词。另外**带引号的多词短语改走 `PhraseQuery`**（`phrase_query_on`），代价是所有 jieba 字段从 `WithFreqs` 升到 `WithFreqsAndPositions`——这才是版本号必须涨的实质原因，加字段本身不必。**这三步一条测试都没写**，见 §2026-09-28 复核 末尾那段。782 条 core 测试全过，但没有一条是为此而过的

剩下的按这个顺序做。当初这一行是拿六条证据写的"R2–R7 一项未动"（`INDEX_VERSION` 仍为 2、`prepare_cached` 全仓零命中、`Page<T>` 仍只有 `total|items|truncated`、`AssetQuery` 没有任何宽高字段、颜色阈值仍硬编码 0.2、`refill_rows((*cells).clone(), …)` 仍在），随后同日 R1–R7 全部落地，这份清单就变成了历史；**09-25 逐条重跑，只有两条还成立、三条已经反过来**，留在这儿是因为"证据行"和结论一样会过期：

- ✅ `INDEX_VERSION` 已 bump 到 **5**（`search.rs:60`；09-27 到 3 是元数据 facts 十字段，09-28 的 4 是逐面拼音、5 是 `audio` 字段，并且全部 jieba 字段为支持短语位置匹配改存词位置）。
- ❌ ~~`prepare_cached` 全仓零命中~~ —— **这句话在它自己的下一段就被推翻了**：`store/rows.rs:31`、`:49` 两处已在用（`rows.rs:21` 的注释正是解释这件事）。本文上一版在 R3 后半记完"换成 `prepare_cached`"之后又留着这条零命中，是前后矛盾。
- ❌ ~~`refill_rows((*cells).clone(), …)` 仍在~~ —— 现在 `panels/workspace/mod.rs:980` 传的是 `&cells`，构造器签名在 `app/rows.rs:165`。
- ❌ ~~`AssetQuery` 没有任何宽高字段~~ —— R5 落了 `resolution`（`ResolutionBand`）。
- ◐ `Page<T>` 仍是 `total|items|truncated`（`model/query.rs:192-202`）—— 字面成立，但 `truncated` 的语义在 R2a 之后已经变成"只在撞 `MAX_RANKED_POOL` 天花板时才真"。
- ◐ 颜色那条仍成立但**含义变了**：`MIN_SIGNATURE_SIMILARITY = 0.2` 还在（`store/visual_search.rs:148`，上一版引的 `:178` 已经漂走），它现在只是"相似度扫描的地板"，与 0–100 那颗滑杆不是一回事——`visual_search.rs:173-174` 的注释就写着"它*不是*分数阈值"。

| 阶段 | 做什么 | 为什么排在这 | 量级 |
|---|---|---|---|
| ~~**R1**~~ | ~~分页会话：core 侧持一份有序 id 快照 + 游标，总数与快照分开取~~ **已完成（2026-09-24）**：`BrowseSession`，排名型视图冻结 id 列表、集合型视图仍由 SQL 开窗，网格改从快照切页并删掉去重兜底 | 它给 R2 的索引内过滤和 R3 的偏移分页提供了挂载点 | — |
| ~~**R2**~~ | ~~把可过滤列镜像进 Tantivy 做**索引内过滤**，让"先筛后排"次序反过来~~ **假阴性已按 R2a 根治（2026-09-24，用户选的这条路）**：有过滤条件时池子取回词项的全部命中，`MAX_RANKED_POOL = 200_000` 只是天花板、且撞到它就明说（`truncated`）。**R2b（镜像列 + 再一次 `INDEX_VERSION`）**：版本号已在 **09-27→3、09-28→5** 连涨两次，但那两次涨的是**文本面**（facts / 逐面拼音 / 音频规格），**可过滤列镜像仍未做**，也只剩性能意义：全量 gather 的大头是 `collect_ids` 每候选一次 stored-doc 读（100k 文档 debug 构建 1.02 s），要压下去需要把 `asset_id` 变成 fast field——那才需要再重建一次索引 | 当初写"唯一根治"是把它和"次序"绑在一起看的；次序不动也能消灭假阴性，只是每次冻结要多付线性成本 | — |
| ~~**R3**~~ 前半 | ~~服务"过滤 + 排序"的偏索引~~ **已完成（2026-09-24）**：schema v17→v18 建五条 `(列 DESC, id ASC) WHERE trashed_at IS NULL` + 一次 `ANALYZE`，`Store::ensure_statistics` 按行数漂移重跑统计。默认浏览一页从 **188 ms → 1.9 ms**（100k 库，offset 40k）。**测出来的真正病根**：光有索引没用，没有统计时 SQLite 宁可走 `idx_assets_trashed` 再全库排序；`PRAGMA optimize` 也不行（抽样把 `trashed_at` 估成选择性索引），只有全量 `ANALYZE` 会读对 | 见上表"覆盖索引"行 | — |
| ~~**R3** 后半~~ | ~~把 `rank_intersect` 那句 SQL 文本随候选数变化的 `id IN (?,?,…)`（最长两万个 `?`）换成**文本固定**的形状~~ **已完成（2026-09-24）**：候选 id 改走**一个 JSON 数组参数**（`id IN (SELECT value FROM json_each(?))`），`rows.rs` 的两处 `prepare` 换 `prepare_cached`。**纠正这一行原来的说法**：`prepare_cached` 单独加是没用的——语句按文本缓存，而这条语句的文本每查必变。20000 候选实测 214 ms → **54 ms**，其中 137 ms 原本是"解析两万多个占位符"这件事本身 | 冷页的排序成本已由前半解决；这一笔是搜索路径上最后一个 O(池宽) 项 | — |
| ~~**R4**~~ | ~~命中高亮：把 snippet 随结果回传，卡片与检查器标出命中的词~~ **已完成（2026-09-24）**：不走 snippet，`search/highlight.rs::Lexicon` 从已解析的 `Atom` 出字节区间，列表行用 `StyledText::with_highlights` 上色；网格磁贴本来不画文字，所以标的就是列表行 | 独立、可见、便宜；排在性能项之后只因为它不改变能查到什么 | — |
| ~~**R5**~~ | ~~分辨率分档 chip（1K <2240 / 2K 2240–3199 / 4K ≥3200）~~ **已完成（2026-09-24）**：`ResolutionBand` 按长边分档，工具栏独立一颗 + CLI `--resolution` + 智能集合同一条下推，真库三档实测 117 / 6 / 3 | 现在只有 6 个宽高比预设 + 方向，缺的是"按像素档筛" | — |
| ~~**R6**~~ | ~~颜色筛选：HSL 匹配盒 + 0–100 相似度滑杆，并把 `extra LIKE '%visual_phash%'` 全表扫换成 SQL 预筛~~ **已完成（2026-09-24，除落库列）**：`Hsl` / `ColourMatch`（三轴区间按相似度插值、最差轴定分、圆周取短距、中性规则非对称）+ 取色器弹层里的 0–100 滑杆（松手提交）+ 分数上屏（列表视图一列）+ 检索改读调色板、老资产退回直方图。实测 release 50k 签名图 **671 ms → 175 ms**。**"换成 SQL 预筛"这一条按原样做了反而是错的**：量下来剩下的成本是每行 1.1 KB 直方图文本必须先读进来，所以要它下降只能把 HSL 提成真实列——那是一次重写 `assets` 全表的迁移，这一轮不做，见上表"颜色筛选相似度"行 | 相似度语义**不需要**新落库字段（原判断错了：不新增字段也能问对），需要新字段的是"更快"而不是"更对" | — |
| ~~**R7**~~ | ~~布局侧收尾：追加一页时不再克隆全部已加载 cells~~ **已完成（2026-09-24）**：`Row` 的 cells/widths 换成 `Rc<[…]>`，追加走 `rows::appended_rows` 复用头行，每窗 O(已加载) → **O(新页)**；三个行构造器改借 `&[Cell]` | 查询与物化已经按窗口恒定成本，这是同一条路径上剩下的那笔 | — |
| **纵深索引** | `INDEX_VERSION` 2→**5**：facts 十字段（09-27）+ 逐面拼音四字段 + `audio` 两字段与五个 qualifier + 带引号多词短语走 `PhraseQuery`（09-28，代价是全部 jieba 字段要存词位置）。见本节编号第 13 条与 §2026-09-28 复核 | 颜色相似度（R6 落库列）是下一个候选；R2b 的 fast field 镜像只剩性能意义 | S · **补测试收尾 S** |

**建议不追**：Serpent 的会话 LRU 32（多快照缓存）。桌面端同一时刻只有一个可见列表，一个当前会话 + 一次失效就够，多快照是为浏览器后退键服务的。

---

## E. 组织与元数据

| 差距项 | Serpent 参照 | Trove 现状 | 量级 |
|---|---|---|---|
| **持久化撤销** | 三张表 `operation_history`（`library-service.ts:3347`）/ `_steps`（`:3362`）/ `_attempts`（`:3376`）：步骤携带**受 schema 约束的正向与逆向配方**（`src/worker/operation-history-recipes.ts:24-40`、`:111`）、逐方向前置条件，attempts 让多步变换崩溃后可续，`historyPolicySchema = ['reversible','barrier']`（`operation-history.ts:18`）让 barrier 在不可逆提交时截断 redo 与过期依赖（`:88-94`），`stale` 是**状态而非异常**（`:11`）；表在库自己的 SQLite 里、由 Worker 持有，Desktop / 脚本 / MCP / 插件共享同一份历史。六条全核实 | 内存栈，`DEFAULT_UNDO_CAP = 20`（`history/undo.rs:22`），`with_cap` 写好了但**无人调用**（`:345`、`:439`）。已经对的部分：每个 `Op` 变体都带 before/after 配对，即**逆向配方本来已有**（`undo.rs:111-174`，如 `PatchAsset{before,after}`、`SetTrashed{before,after}`），不可逆集合也明确声明（`library.rs:896-898`：purge / 清空回收站 / 导入 / 删标签与删合集不记录），还有一层带 i18n 键的人类可读描述（`OpAction::key` `undo.rs:51-70`）+ 状态栏 5 步 undo / 3 步 redo 历史弹层（`app/status_bar.rs:34-42`、派发 `app/root.rs:1085`、`:1096`）。**上一版说 `with_cap` "写好了但无人调用"，这条已经不成立**：栈深现在是配置项——`config.rs:56 pub undo_cap: Option<usize>`，读的时候 `:782-783` 取 `unwrap_or(DEFAULT_UNDO_CAP).clamp(1, 500)`，真库那条路径在 `library.rs:319` 就吃它（`SharedUndoStack::with_cap(AppConfig::load().undo_cap())`），内存态那条仍用 `DEFAULT_UNDO_CAP`（`library.rs:400`，常量在 `undo.rs:22`）。**但它只到配置文件为止**：`grep -rn "undo_cap" crates/trove-app/src` 零命中，所以没有任何界面能改它，设置页八个字节这一格是空的。缺的三样仍是那三样：**不落库**（`undo.rs:8-10` 的模块注释原话就是 "deliberately not persisted"，13 张表里没有任何一张记历史：`schema.rs` 的 `assets:328` `collections:409` `asset_collection:424` `tags:435` `asset_tag:443` `smart_collections:456` `view_history:475` `search_queue:517` `model_looks:227` `asset_sequences:135` `asset_sequence_frames:145` `search_queue:517` `asset_embeddings:559` `ai_analysis:312`；`grep -rniE "op_history\|operation_history\|undo_log" crates/` 零命中）、无 attempts 续跑、无 barrier 与结构化 `stale` | L（外加一格 S：把 `undo_cap` 挂上设置页） |
| 标签合并 | create/rename/**merge**/delete/delete-many + 共现图喂建议（四条命令与 `tag.cooccurrence` 都在 `src/automation/command-registry.ts` 里，逐条核实存在） | 层级（`subtree_ids` `store/tags.rs:75`、`move_to` `:90`）、色标（`set_color` `:292`）、**递归子树计数**（`count_assets` `:159`、`counts_by_tag` `:198`）都有，全套 **16** 个 `pub fn`（`:27 create` `:54 get` `:64 get_by_name` `:75 subtree_ids` `:90 move_to` `:115 ensure_named` `:134 list` `:144 for_asset` `:159 count_assets` `:198 counts_by_tag` `:220 add_to_asset` `:230 remove_from_asset` `:240 set_for_asset` `:258 delete` `:272 rename` `:292 set_color`；上一版说"17 个"，逐个数下来是 16 个）。**独缺两样：合并与批量删除**——`delete` `:258` 只吃单个 id；`rename` `:277-281` 遇到重名是**报错**（`tag `{name}` already exists`）而不是并句柄，所以连"改名撞车即合并"这条最省事的搭头都没有；`grep -rniE "delete_many\|remove_many\|bulk_delete" crates/` 零命中；全仓每个 `merge` 命中都与此无关（`media/index/sort.rs`、`model/query.rs:143-161`）。UI 侧菜单只有 filter/new-child/rename/color/delete/move（`panels/tags_panel.rs:344-400`），CLI 的 `tag` 只有 add/remove/replace（`trove-cli/src/write.rs:276-284`）。`TAG-COLLECTION.md:70` 声称有合并，是文档失真。**09-28 在同一个输入框上加的是层级链不是合并**（`library.rs:1471 create_tag(name, parent)` 逐级当父级，见 §H 的标签层级链行），`grep -rniE "merge_tag\|tags::merge\|delete_many" crates/` 本轮重跑仍**零命中**，所以"合并"与"批量删标签"这两样一格都没动 | S |
| 合集封面 | `collections.cover_asset_id`（`library-service.ts:1309`） | 无该列：`collections` 只有 `id, parent_id, name, appearance, position, created_at, updated_at` 七列（`schema.rs:409-419`），`cover_asset\|custom_cover\|collection_thumb` 全仓零命中。**已有一半**：v16 起 `appearance TEXT` 存每容器自己的字形与强调色（`schema.rs:240-241` 的 `ALTER TABLE … ADD COLUMN appearance`，集合与智能集合各一条；建表里在 `:415`），缺的是"拿某个资产当封面" | S |
| 批量改名落到文件 | `asset.rename-files`：单次 `.max(10_000)` 条（`src/automation/command-registry.ts:661`），跳过原因恰好 **4** 种——`asset_not_found` / `asset_unavailable` / `name_conflict` / `invalid_name`（`:1558`）；链接式文件夹里改名**就是**改源文件 | `Library::batch_rename`（`library.rs:916-954`）展开 `{n}` / `{name}` 之后**只**做一件事：`assets::update(AssetPatch{ title })`（`:933-941`）+ `undo.record(Op::SetTitles…)`（`:943`）。全函数没有一次 `std::fs::rename`，也不写 `file_name` 列——**磁盘名永远不动**（UI 入口 `dialogs/rename.rs:97`） | M |
| 托管文件夹 | `managed_folders` 真实目录树（`library-service.ts:1107`）+ `linked_folders`（`:1191`）+ **可单向转托管**（`src/renderer/ConvertLinkedDialog.tsx`） | 模型是"链接 + 内容寻址"两态，不是目录树：`assets.origin ∈ {stored, linked}` 配可空 `rel_path`（`schema.rs:333-337`），链接原件在 `extra.source_path`（建表注释 `:325-328` 就写着这件事、`model/facts.rs:151-155`）；`ImportStorage::Link` 原地不动，`Copy` 写的是**按内容哈希的 blob 桶**而非文件夹树（`media/import.rs:369-371`、`:450-458`）。`services/storage.rs` 名字唬人，它只报磁盘占用（`:1-9`、`report()` `:84`）。`convert.*linked\|make_managed\|import_mode` 零命中 → **没有转托管这条路** | 与"只链接"哲学冲突，见 §H |
| 被忽略项可还原 | 忽略后从 UI/搜索/扫描消失但 **DB 元数据保留**，反忽略即还原（表 `linked_ignored_assets`，`library-service.ts:1611`）；另有 `.serpentignore`（读取在 `:5526`，v26 建的表 `:3325`）+ 每链接文件夹结构化规则 + 菜单显式忽略三层 | 直接读 git 原生规则：`.gitignore` / `.ignore` / `.git/info/exclude`，优先级同 git（`tasks/ignore.rs:1-45`），事件路径上的缓存随设置周期丢弃。命中即**完全跳过**：`schema.rs` 里没有 `ignored` 表或列，没有可还原的元数据，也没有"每文件夹结构化忽略规则"这层 | M |
| 元数据字段 | `author`、`palette`（≤12 自动色卡 + 人工覆盖优先） | `author` **全仓零出现**。没有 `palette` 字段/列，最接近的是 `facts.visual.dominant_colors`（`model/facts.rs:48-61`）。反过来 Trove 有 Serpent 没有的：`usage_status` 与三态 `commercial_use`（`assets` 列 `schema.rs:363-364`）、`facts.photo` 8 个拍摄字段（含 GPS）、`facts.font` 5 个字体字段、`facts.audio` 4 个采样字段（`:21-133`） | 打平偏 Trove |

---

## F. 编辑与维护

| 差距项 | Serpent 参照 | Trove 现状 | 量级 |
|---|---|---|---|
| 回收站保留期 | 30 天**写死在两处**（`library-service.ts:38593`、`src/worker/catalog-read.ts:724`）+ 逐资产剩余天数（`catalog-read.ts:721-725`、`src/shared/asset-types.ts:197`）+ **整个托管文件夹**可入回收站（`trashed_managed_folders` 表，`library-service.ts:1820`）+ 被别的程序占用的文件**跳过并回报**：purge 返回 `skippedCount` 与 `failures[{assetId, reason}]`（`:38589-38590`），下一轮 `purgeExpiredTrash` 再试（`:30037`）——注意是**下一轮**，不是同一轮内重试 | 回收站 / 恢复 / 永久删 / 清空都有（`trashed_at` 列 `schema.rs:360`；`purge_asset` `library.rs:864`、`empty_trash` `:873`、批量 `purge_assets` `:1850`）。缺的三样，09-25 逐条重跑 grep 仍成立：全仓 `grep -rniE "retention\|auto_purge\|days_remaining" crates/` **零命中** → **无保留期、无到期清理**；入回收站是逐选中的 DB 翻标记（`store/batch.rs:16` `set_trashed_many`，`library.rs:1017-1023` 的 `trash_assets`/`restore_assets` 都只吃 asset id），**文件夹不能作为容器入回收站**（合集是硬删：`panels/explorer.rs:1229` 直接 `collections::delete`）；真正删文件那一处仍是 best-effort——`library.rs:1916`、`:1919` 两个 `let _ = std::fs::remove_file(...)` 把错误整个丢掉，被占用的文件**静默留在原地**，既不提示也不排下一轮。**同一件事在收件箱那头做对了**：`services/collect.rs:184-202 remove_inbox_file` 会 `match` 错误、非 `NotFound` 就 `tracing::warn!` 并返回 `false`——所以这不是"作者不知道要看错误"，是只有入库那侧有人接。因此这一条真正的两笔是：**保留期 + 把 `remove_file` 的错误接起来**是 S（一个数字、一处 `match`；`PurgeReport` 已经有四个计数器 `purged/blobs_removed/thumbs_removed/sources_removed`（`library.rs:174-180`），要接 Serpent 那个 `skippedCount` + `failures[]` 有现成的地方放，只是今天它只数成功），**文件夹作为容器入回收站**是 M（要新表、要让 `build_where` 认容器身份） | S + M |
| 自动更新 | `src/main/app-update-service.ts` + `src/shared/app-update.ts`：下载 + sha256 校验（`:136`、`:197`、`:352`）+ installer/portable 按发行形态匹配（`:203-242`）+ `mandatory: z.boolean()` 强制更新标记（`:43`）+ `onDownloadProgress`（`:106`）与 `cancelDownload()`（`:507`）+ **11 种错误码**（`app-update.ts:13-24`，数过）+ 更新后重启/拉起安装器（`:105`）。全部核实 | 只查版本不下载，且注释明写意图：`services/update.rs:1-8` "Read-only by design … it never downloads or replaces anything"（`check_now` `:102-113`）。是有意的，但意味着用户要手动换包 | M |
| 打包成安装器 | Forge：`forge.config.ts:133-134` 的 `MakerZIP(['darwin','win32'])` + `MakerDMG`；Inno Setup 走 `scripts/inno-build.mjs` + `assets/inno/serpentsetup.iss`——语言选择页 `:36`（`[Languages]` `:45` 里连简体中文都有）、路径页 `:3`、`PrivilegesRequired=admin` 即 UAC `:39`、自动卸载器 `unins000.exe` `:4` | `.github/workflows/release.yml:23-44` 四个目标：两个 macOS（aarch64 / x86_64 `.zip`）、`x86_64-unknown-linux-gnu`（`.tar.gz`）、`x86_64-pc-windows-msvc`（`.zip`）；`:100-117` 只打包裸二进制，`:146` 的发布 glob 就是 `artifacts/**/*.{zip,tar.gz}`。全仓唯一的打包文件是 `packaging/linux/trove.desktop` ——**没有 msi / Inno / dmg / deb / rpm / AppImage / flatpak**，也没有 Justfile / xtask / dist。Linux 侧还有一条硬运行时要求：产物在 ubuntu-24.04 上构建，因此要求 **glibc ≥ 2.39**（即 Ubuntu 24.04+ / Debian 13+，`release.yml:35-37` 的注释），而这条只写在 CI 注释里 | M |
| 损坏库救援 | `library.recovery-report`：备份 → 物理 `Assets/` salvage 并给出可恢复文件数 | 分四层说清（09-25 再核，并且**记下这一行差点又被自己骗了**：在 Trove 里 grep `integrity_check` 是会命中的，命中的却是下面②那套*内容级*校验的函数名，不是 SQLite 的 pragma）：① `PRAGMA integrity_check` / `PRAGMA quick_check` **仍不存在**（`grep -rn "quick_check" crates/` 零命中；`store/mod.rs` 真正执行的 pragma 只有 `journal_mode:47` / `synchronous:54` / `cache_size:57` / `foreign_keys:83` / `user_version:220`、`:225`，统计那条是 `schema::analyze_statistics`（定义 `store/schema.rs:214`，开库时在 `store/mod.rs:138` 调、迁移尾巴挂在 `schema.rs:83`）；`PRAGMA optimize` 在这儿只是**注释**，解释的是为什么不用它）——**结构级损坏无人问**；② 存在的是**内容级**校验：`services/maintenance.rs` 的 `IntegrityReport:418` / `plan_integrity:437` / `run_integrity_plan:464` 逐个 blob 对哈希、缺失即入回收站，**并且它在界面上有一颗按钮**（`dialogs/settings/files.rs:746` 派发 → `:845 run_integrity_check`，走 `spawn_maintenance_job`，结果写回 `ctl.integrity_report` 并用 `settings.verify_done_*` 报数）；③ 每次开库自动 `VACUUM INTO` 快照、24h 间隔、滚动留 10 份（`services/backup.rs`：`list_backups:27`、`create_backup:42`、快照语句在 `:54`），设置页列得出来也能 reveal 所在目录（`dialogs/settings/files.rs:386`、`:427`），**但没有任何一个"从快照还原"的函数**（全模块公开面只有 `backups_dir:22` / `list_backups:27` / `create_backup:42` / `prune_backups:62` / `maybe_auto_backup_at:73` / `maybe_auto_backup:93`）；④ `trove doctor`（`cli.rs:106` → `read.rs:603`）只报健康不修。库文件本身开不了时确实无路可走，而 ①+③ 合起来只差一个函数：**开一份只读副本跑 `PRAGMA integrity_check`，再把 `backups/` 里那份快照写回原位** | M |
| 介质自适应 | 本地/可移动盘 WAL、确认的网络挂载 **DELETE 日志** + `synchronous=FULL` + `busy_timeout`；写 lease + 持久变更序列；**NAS 掉线立刻转只读**，写前复验 | 一次 `execute_batch` 定全部：`journal_mode=WAL`、`synchronous=NORMAL`、`cache_size=-16000`、`busy_timeout` 5s，而且**返回值全部用 `let _ =` 丢弃**（`store/mod.rs:46-59`）——pragma 有没有生效无人知晓。没有任何文件系统类型 / 网络挂载探测 | M |
| 工件自愈 | `derived-artifact-repair.ts` 重排可再生产物（接触表刻意排除，让失败保持终态） | 单件懒生成的函数**已经有了**：`thumb::ensure`（`media/thumb.rs:32`，另有 `ensure_for_asset:66`、`regenerate:79`、`abs_path:23`；头注释明写"缺失或损坏的缩略图绝不使调用方失败"）。问题是**渲染路径不调它**。09-25 把调用方数全了（上一版漏了两处，写成"只有 `embed.rs` 与 `ai_analysis.rs`"）：`media/pipeline.rs:747`（导入期 `ThumbStage`）、`tasks/embed.rs:73`、`tasks/ai_analysis.rs:840`（后两处都是 `ensure_for_asset`）、`media/hdr.rs:274`（测试内）、以及两个 example（`examples/font_card_preview.rs:18`、`examples/import_profile.rs:131`）——**没有一个是界面**。渲染侧 `panels/workspace/data.rs:515-519` 是 `abs_path(cache_root, hash)` 后面接一句 `.filter(\|p\| p.is_file())`，落不到就是 kind 图标（列表行的同一件事在 `cells.rs:313 list_lead_fallback`，调用点 `:386`），于是"看到缺图"永远不等于"补图"，要补得跑整库 `rebuild_thumbnails`（`maintenance.rs:269`，计划 `plan_thumbnail_rebuild:199`）。**这一条只差在视图路径上调那一次**，而且 `ensure` 的签名要的是 `(root, sha, kind, blob_path)`——`data.rs` 那一句手里的 `Asset` 三样都有。**但"照抄一行"是错的**：`ensure` 会整图解码 + 缩放，而它在导入期就是靠 `proc::slot()` 之外的后台线程跑的，渲染路径在 `App` 线程上，直接调等于一次滚动触发十次全解码。便宜的做法是把这一格交给已有的任务系统（缺图即排一个 `ensure`，卡片先画 kind 图标，图到了再刷那一格），而不是在 `data.rs` 里同步调用 | S–M（取决于是否复用 `TaskManager`） |
| 整包导出 | 库导出 folder \| zip，**显式不开 ZIP64**，>4 GiB 或 >65,535 条目提前拒绝并提示改导出文件夹 | `zip` 2 + deflate + **`large_file(true)` 开着**（`services/archive.rs:89`、`:208`），条目流式 `std::io::copy`（`:95`、`:251`），`library.db` 走 `vacuum_snapshot` 一致性快照（调用 `:230`、定义 `:295`），失败整包删除。**没有任何体积 / 条目数预检**——09-25 把该数的都数了：`available_bytes`、`disk_space`、`free_space`、`too large`、`MAX_ENTRIES`、`entry_count` 六个词在 `archive.rs` 里**零命中**，所以"要写多大、装得下吗"这个问题今天没人问，上限行为仍未验证。**但别照抄 Serpent 那条拒绝线**：它显式**不开** ZIP64，所以 >4 GiB 必须拒绝；Trove 恰恰因为 `large_file(true)` 开着，>4 GiB 是**写得住**的，真缺的是"磁盘快满了"这一半——一次 `statvfs` 就够，不需要条目数，也不需要新依赖 | S |
| 无障碍 | `aria-*` 实测 **554** 处、分布在 114 个文件（本表上一版写的 ~521 偏低）；dialog 焦点陷阱 `use-dialog-focus-trap`、`prefers-reduced-motion` 5 处（`styles.css`）、roving tabindex `roving-list-keyboard.ts:3`、`:61`、IME 安全关闭 `ime-safe-dismiss.ts:12-21`、`:66` | 全是自绘渲染，`aria-*` 在这套框架里没有对应物；能对照的只有逐组件的焦点与可达名（例：`panels/workspace/toolbar/color_filter.rs:13` 明写 ColorPicker 自带焦点与 Enter/Escape）。没有可达性审计、没有动效减弱开关：主题的 `motion_tokens()` 全仓**只用在一处**（`app/dock_skin.rs:705`，拖放占位块的弹簧），`reduced_motion` / `reduce_motion` / `prefers_reduced` 三个词在 `crates/` 下零命中，`config.rs` 里连 `motion` 这个字都没有，所以既没有开关也没有配置项。**这一条的量级还要再往上游推一层**：gpui 是自绘、没有 a11y 树，`aria-*` 在这里没有对应物，逐项补等价能力不是"补文案"而是先要一个 accessibility 后端 | L（含上游依赖） |

---

## G. 平台与集成

| 差距项 | Serpent 参照 | Trove 现状 | 量级 |
|---|---|---|---|
| 屏幕截图 | ❌ 确认完全没有：`src/` 下没有 `desktopCapturer`，也没有任何屏幕采集路径；唯一的 offscreen `capturePage()` 是给 HTML 缩略图用的，采不到桌面 | ✅ **7 个目标**（Workspace / Screen / ActiveWindow / Window / Area / PickWindow / PickArea，`CaptureTarget` 定义在 `services/screenshot.rs:35-60`）× 进程内 2 路（KWin `org.kde.KWin.ScreenShot2`、`xcap`）+ 平台工具 4 路（macOS `screencapture`、Windows PowerShell、`grim`+`slurp`、`scrot`）。整张矩阵写在 `services/screenshot.rs:14-22` 的表里，`plan_for` 是纯函数，所以**无显示环境也能单测**；KWin scripting 只借它取窗口列表（`services/kwin_script.rs`、`app/capture.rs:155-161`）。含自绘框选吸附（`components/capture_pick.rs`） | **Trove 领先** |
| 单实例 | `requestSingleInstanceLock` + 文档化的多开逃生口 | 无：`single_instance\|flock\|LockFile\|CreateMutex\|pidfile` 全仓零命中。**唯一像样的旁证是代码已经预期了多开**——`app/root.rs:1137-1139` 专门处理采集端口被占的那种情况，注释写着 "Port taken (another instance?)"。第二次启动就是再开一个进程。缓解项是既有的只读并发（桌面版持库时 CLI 只能读，见 §H），所以多开至少不会打坏库 | S |
| macOS 原生菜单 / Windows 无边框控件 | 有 | ✅ **原生菜单条已经有**（2026-09-24 复核，此前本行低估）：gpui-kit 的 `AppMenuBar`（`app/title_bar.rs:11`）+ `cx.set_menus(build_menus())` 与 `GlobalState::set_app_menus`（`:60-69`），四个菜单 File / Edit / View / Help（`build_menus` `:71-166`），**随语言切换整体重建**（`:186-201`）。剩下的是平台习惯层：没有 macOS 专属应用菜单约定（无 `MacOsAppState` 之类代码），Windows 无边框自绘标题栏未逐平台验证 | S |
| Linux 发行 | ❌ 不是"没做"，是**主动拒绝**：`currentPlatformKey()` 对 darwin-arm64 / win32-x64 之外的平台直接 throw（`scripts/media-binaries-lib.mjs:40-51`，另一处 `scripts/release/lib.mjs:58`） | ✅ 三平台 CI（4 个目标，见 §F 打包行） | **Trove 领先** |
| 界面语言 | **2**：`src/renderer/i18n/catalogs/en.ts` 与 `zh-CN.ts`，逐文件核过 | **9** 份 locale，`en.toml` **784** 个叶子键（上一版记 779；本轮 +5：`inspector.append_tags_flat_hint`、`inspector.append_tags_chained_hint`、`settings.audio_card_waveform`、`settings.audio_card_waveform_desc`、`task.kind_custom`，并把 `inspector.add_tag_placeholder` 的文案从"添加标签 + 回车"改成"标签，逗号分隔 + 回车"。数法与棘轮测试**用的是同一套**：`app/i18n.rs:118-134` 那个 `keys_of`，按 `[section]` + `key =` 归成 `section.key` 去重）。09-28 逐键重测，结果与 `app/i18n.rs:14-24` 声明和 `:106-115` 的棘轮表**完全相符、无漂移**：**de 缺 64 / 多 2，es·fr·ja·ko·pt·ru 各缺 63 / 多 1，zh-CN 与 en 完全同步**——本轮五把新键九份一起加了，所以 allowance 一格没动、`cargo test` 里那条棘轮测试仍绿。缺口的组成：`settings.chat_*` 17 条、`settings.ai_*` 16 条、`settings.search_*` 11 条、`autotag.*` 11 条、`settings.autotag_*` 5 条、`settings.embed_multimodal*` 2 条、`inspector.content_hash` 1 条、`shortcuts.actions.AutoTag` 1 条，de 另缺 `settings.theme_dir`。"多"的是过期键：八份都留着已废弃的 `settings.ai_embedding`，de 还多一个 `settings.theme_theme`（本轮这两类**一格没变**，所以 allowance 用不着调）。**另有两个本轮新产生的死键要单独记一笔**：`inspector.replace_tags_hint` / `inspector.replace_tags_failed` 现在**九份都有、而代码里已无人引用**——它们在 `en.toml` 里也有，所以按棘轮测试的"相对 en 的多余键"口径**不计数**，棘轮看不见它们，得靠人记得删（见 §I）。这些位置回落英文 | 数量领先，**覆盖不均**。棘轮按语言记录当前缺口，新增不回补即红 |

---

## 内容类型计划（§C，2026-09-24 定序）

本轮（09-24）把 §C 的九行逐条对着两边源码重核了一遍，量级有三处改动，并新发现一处**文档与代码不符的真实缺陷**。排序依据三条硬事实：只有序列帧动 schema、只有"文本内容入索引"动 `INDEX_VERSION`、EXR 与编码探测需要的 crate **已经在依赖图里**。

**已定的三条方向**（决定默认值，后续几轮都按这个走）：

- **序列帧 = 手动为主 + 导入后提示**。导入时算候选、逐组确认，另给「创建 / 解散序列」命令作用于多选（≥3、同目录）。这与 Serpent 的实际默认档一致（`image-sequence-preferences.ts:19` 的 `autoDetectOnImport: false`——它自己的 `docs/user-guide/basics.en.md:53` 把这条写成"默认开"，又一处文档失真）。**不做**导入即成组：那是"只链接"库里最大的一次行为改变，看不见的不等于不在。
- **悬停 = 做，默认开**。同时只 1 张活卡片（Serpent 的这条约束必须一样，见 `use-asset-card-hover-preview.ts:46-67`），500ms 去抖，视频走关键帧近似 seek、无 ffmpeg 自动退回静态图。
- **PDF 首页缩略图本轮不做**。§C 那一行留在原地，路线已经比过：外部光栅器链（`pdftoppm`→`mutool`→`gs`，与 `heif-dec`/ffmpeg 同构，零依赖）为第一候选，随包 pdfium 是保真度换包体的备选。HTML 那半**改道**：`.html .htm .xml .css .js` 本质是文本，文本查看器给它们真内容与真卡片，比 Serpent 的 offscreen 截图更有用，也免掉一整套沙箱与超时机器（它的 `document-thumbnail-renderer.ts:63-75` **故意不设渲染截止**，失败静默不写记录）。

| 阶段 | 做什么 | 代价 | 量级 | 状态 |
|---|---|---|---|---|
| ~~**T1**~~ **已完成（2026-09-24）** | 波形带拖拽 seek：带上加 `on_prepaint` 记左边距 + 按下/移动/松开三个处理器，落点与滑块**同一个** `scrub_to(ms, commit)`（`preview/audio.rs`），拖动时在带上画一条播放头。**视频截当前帧入库**：`media/video.rs::write_frame_png(path, at_ms, out)` 一次 ffmpeg 出**源尺寸**的 PNG，落 `paths::incoming_dir()` 再走 `jobs::import_paths_app`（与截图同一条链），预览工具条一颗相机按钮（`has_video` 才出现）。**两处和当初的写法不同，都是有意的**：没有去取屏幕上那块 `shown` 缓冲——播放器按 `DEFAULT_MAX_WIDTH = 720` 解码，取它就等于把 720 宽的播放预算当成一张要留下来的图（也就不需要那次 BGRA 换序）；也没有用 `import_copied_app`，因为 `incoming/` 就是这类自产文件的常驻地，链接它才对 | 零依赖、零迁移、零索引 | S×2 | ✅ |
| ~~**T2**~~ **已完成（2026-09-24）** | EXR / HDR / TGA 三特性 + 后缀与 mime + `media/hdr.rs` 的显示曲线（详见 §C 那一行的落地记录）。**没做 ±10 档滑杆**：常驻一个 4K 浮点缓冲约 500 MB，而 Serpent 每次改档重跑一遍 oiiotool，两条都不划算，见下面「明确不追」 | `exr` 1.74.2 已在 `Cargo.lock:2170`；`hdr`/`tga` 零依赖；测试为写真实 EXR 加了 `exr` 的 **dev-dependency**（同版本，已在图里） | M 偏 S | ✅ |
| ~~**T3**~~ **已完成（2026-09-24）** | **文本查看器 + 编码探测**：`media/text.rs` 判据逐条对齐 Serpent `text-encoding.ts:177-233`（BOM → UTF-16 NUL 奇偶 → 严格 UTF-8 → chardetng；NUL >2% 判二进制）；`is_text_ext` 单一清单（**96** 个后缀）；`preview/text.rs` 用 gpui-kit 的 `Editor::new(state).readonly(true)`。**截断必须挡住保存**这条用另一种方式满足了：查看器根本不写。**三处它没有的细节**：UTF-32 的 BOM 显式排除（否则解出一串像模像样的假中文）、截断点落在半个字符上时先裁尾巴再判 UTF-8（否则正常 UTF-8 被误报成 legacy）、1 MiB 的读法是先多读一字节再裁回上限（`truncated` 才是知道的而不是猜的） | `chardetng 1.0.0`（纯 Rust）+ `encoding_rs 0.8.35`（已在 `Cargo.lock:2002`） | M 偏 S | ✅ |
| ~~**T4**~~ **已完成（2026-09-24）** | **文本卡片**：`thumb::ensure` / `regenerate` 的 `_ => None` 臂先问 `is_text_ext`，卡片画的是文件**自己的前 11 行**。**没有用 fontdue**：改成生成一小段 SVG 再走 `render_svg_data`（从 `render_svg` 里分出来的），因为那是本 crate 唯一一处"解析字体家族并会 fallback"的地方——自己排字的话，一份中文文件就是一排豆腐块。卡片仍按内容哈希寻址，所以没有新缓存目录要登记；但 `plan_thumbnail_rebuild` 的 kind 表补了 `Document` / `Other`（按后缀过滤，免得把 PDF/归档算进重建数）。PDF 臂本轮不做，见上 | 零依赖 | S | ✅ |
| ~~**T5**~~ **已完成（2026-09-24）** | 一张活卡片的通道。**没有放在 controller**（`cx.notify()` 会惊动所有观察面板），而是 `WorkspacePanel` 持一个 `Entity<LiveCard>` 并 `observe` 它；`build_cell_element` 多一个参数，`on_prepaint` 与 `on_mouse_move` **只给活卡**。（**2026-09-25**：`on_hover` 整条去掉，触发改成空格）视频解码环 320 宽、mailbox + presenter、EOF 必须丢管重开（否则读 None 空转烧核）。开关 ~~`settings.hover_media`~~（2026-09-25 连同指针触发一起去掉，改空格） | 零依赖 | M | ✅ |
| ~~**T6**~~ **已完成（2026-09-24）** | 接触表补上了生产者：`media/video.rs::write_contact_sheet` 一次 ffmpeg（`fps=16/时长` + `scale=256:-2` + `tile=4x4` + `drawtext` 打时间戳），`ai_analysis` 首次用到时现做、按内容哈希缓存。**画不进就不画时间戳**（`drawtext` 要字体提供者，某些 ffmpeg 构建没有），但那个降级是**静默的**，所以额外有一条测试单独跑带时间戳的那条链——否则转义写错只会让答案丢掉"什么时候"，测试还是绿的。`contact-sheet/` 同时登记进 `clean_orphans`（`sheets_removed`）、`storage.rs`（`cache()` 四个租户）与 `trove paths`（顺带补了漏掉的 `waveforms`） | 零依赖 | S | ✅ |
| ◐ **T7 做了一半（2026-09-24）：数据层完成，UI 未动** | **已落地**：v19 迁移（`asset_sequences` + `asset_sequence_frames(position, frame_number)` 两张侧表，`asset_id` 上 UNIQUE 所以一帧只属于一个 run，全部 `ON DELETE CASCADE`）；`media/sequence.rs` 分组纯函数（尾号/括号两式、**零间隙容忍**、≥3、同目录+前缀+后缀+**补宽分区**——`01..09` 与 `10..12` 是一串而 `001..010` 与 `1..10` 是两串，11 条测试钉着）；`store/sequences.rs` 的 create/dissolve/set_fps/membership，每条拒绝都指名原因（<3、跨目录、尺寸不齐、已属别的 run）；**隐藏帧的那一句**进了 `assets::build_where` 的 live 分支——浏览 / 搜索 / 最近 / 智能合集 / 回收站全部一次覆盖；另外四个绕过 builder 的计数面（`stats::library_stats`、`tags::count_assets` + `counts_by_tag`、`collections::count_assets` + `asset_ids`、`assets::source_folders`）用 `hidden_beside(column)` 各自对齐。**两处刻意的不同**：① 那句只加在 live 分支，回收站分支不加——被丢进回收站的帧必须能单独列出、单独还原，而 `empty_trash` 走同一个 builder，加了就会把 149 个成员的文件永远留在盘上；② 隐藏成员**照样生成缩略图**（Serpent 用 `SEQUENCE_MEMBER` 取消它们），因为卡片轮播与播放器要的就是这些缩略图，取消反而让播放去现解码。**未做（09-25 逐条重验，一项没动）**：`Library` 门面方法（`library.rs` 里三处 "sequence" 全是散文，`grep -rn "create_sequence\|dissolve_sequence" crates/` 零命中，于是 `store/sequences.rs` 那四个函数**在生产代码里无人调用**，只有测试吃它）、CLI、右键「创建/解散序列」、导入后提示、卡片轮播、序列播放器——后四样都在 `trove-app`，而 `grep -rn sequence crates/trove-app/src/` 只有 2 条无关注释，`grep -rni "sequence\|序列" locales/` 零命中（**连菜单文案的键都还没有**）。要补的第一步不是 UI 而是门面：没有 `Library` 那一层，app 与 CLI 都没有可依赖的稳定入口。**这一句 2026-09-28 第二轮起过期了**——门面五颗、右键菜单两颗、CLI 三个子命令都接上了（`locales/` 里九份各 4 条序列帧键），仍没做的是那句里点名的后四样：导入后提示、卡片轮播、序列播放器，以及 `media/sequence::detect` 的调用方 | 🔧 v17→v18→v19（本轮实走），零索引代价 | **L** | ◐ 数据层 ✅ · 手动成组/拆组/帧率 ✅（2026-09-28 第二轮，含门面 + 右键菜单 + CLI）· 导入后提示 / 卡片轮播 / 播放器 0% |
| **T8** | **色彩管理的可追那一半**：`decode_raster` 读 `icc_profile()` → `qcms` 转 sRGB（含 CMYK JPEG），并改掉 `media/edit.rs:8-11` 那句主动丢弃（09-25 原文照录："Re-encoding drops ancillary metadata (EXIF orientation, colour profiles): the pipeline is 'decode pixels, transform, encode'"） | `qcms 0.3.0`，Firefox 的纯 Rust/MIT 实现；`image` 的 png/jpeg/tiff 解码器都已实现 `icc_profile()`（`codecs/png.rs:181`、`codecs/jpeg/decoder.rs:86`、`codecs/tiff.rs:311`，都在用的 **image 0.25.10** 里逐条核过）。**09-25 复核：这一步仍然一步没动，而且它的代价没变**——`qcms` 在 `Cargo.lock` 里零命中（`icc` / `littlecms` / `color_management` 同样零命中），`grep -rn "icc_profile" crates/trove-core/src/media/` 也零命中，所以连"读出来丢掉"都还没做，是彻底的没接线 | M | 下轮 |

**本轮核验改掉的三个估计**：序列帧 M→**L**（旧行只数了"检测和播放"，没数隐藏帧要穿过 5 个计数面——`build_where` 之外还有 `store/stats.rs:31-73`、`store/tags.rs:159-204`、`store/collections.rs:230-247`、`assets::source_folders:928-953`）；文本查看器 M→**S**；EXR/HDR **偏 S**。

**新发现的缺陷（本文上一轮写错了）**：上面的 §C「AI 分析覆盖面」那行说视频分析"带一张**导入期算好**的接触表"——**没有任何代码写这张表**。`tasks/ai_analysis.rs:844-854` 的 `contact_sheet_path()` 只做 `path.is_file().then_some(path)`，全仓 `contact-sheet` 的 12 处命中**无一存在写入方**（连 `create_dir` 都没有）。于是视频分析实际只发了缩略图那一张图，而 `contact-sheet/` 也不在 `clean_orphans`（`maintenance.rs:328`）与 `storage.rs:39-103` 的登记之列——一个没人管的缓存租户。修在 T6：`ai_analysis` 里现做一张（`fps` + `drawtext pts:hms` + `tile=CxR`，行列夹 4…20，一次 ffmpeg、走 slot），并把该目录登记进清理与容量报告三处。**结论：§C 那一行的"已打平"要打折——图像与 3D 是平的，视频差一张接触表。**

**明确不追（下次对比别当成洞）**：HTML 光栅缩略图（T3 覆盖得更好）· ACES / OCIO 工作室配置与逐资产输入空间覆盖表（几十 MB 资产 + 另一套渲染语义，换一条内置 tone-map 曲线）· **EXR 的 ±10 档曝光滑杆**——**这一条 09-25 要改口，但结论不改**。改口的是理由：不是"要做一条曲线"，`media/hdr.rs` 的 `tonemap(image, stops)` 从一开始就吃 `stops`，±10 也写死在 `MIN_STOPS`/`MAX_STOPS`（`:22-23`）并在内部钳位，非零档有测试（`:198-199`），所以**核已经完了，剩的是接线**（两个调用点传死 `0.0`：`hdr.rs:33`、`thumb.rs:506`）。不改结论的是代价：滑杆每动一格就要重解一次全尺寸浮点图（常驻一块约 500 MB 的 4K 浮点缓冲是另一条路，Serpent 每改一档重跑 oiiotool 子进程是第三条），三条都不比它解决的问题便宜。**于是诚实的记法是：一格 S 的 UI + 一条"我们不做实时预览"的说明，不是 M 的活**· EXR plane/part 选择器（Serpent 自己把它藏进 `length > 1` 才出现的 `<select>`）· 序列帧的会话 LRU 与 100k 帧的 IPC 线上限形状（桌面端单窗口单列表，没有后退键要服务）。

**那道门已经开了三次，原来它锁住的三件事现在剩两件**（2026-09-28 更正整段）。`INDEX_VERSION` 现值 **5**（`search.rs:60`）：09-27 涨到 3 时 `artist_words` / `album_words` 进了索引，所以**音频 `artist`/`album` 入索引（P2b）这一件已经做完了**；09-28 的 4、5 加的是逐面拼音与 `audio` 字段。**仍然没做的两件**：① **文本文件的正文**——`media/text.rs` 那套解码器与索引之间今天仍然没有任何一条线（`grep -n "media::text\|read_text\|text::" search.rs` 零命中），而这件事**已经不再拿"要升版"当理由**：升版这件事本轮已经干了三次，缺的只是把正文喂进 `index_asset_text` 的一个字段，代价是每个现存库再清建一次；② **可过滤列镜像进 Tantivy**（R2b）——版本号涨的都是文本面，`asset_id` 仍不是 fast field，所以它仍只剩性能意义（假阴性已在 R2a 根治）。代价不变：一次清建全文索引（派生数据，可自愈，见 `reconcile_search_index`）。

---

## 音频计划（2026-09-23 起，逐项排好的补齐顺序）

**状态（2026-09-28 收尾）**：P1 / P2a / P3 / P4 / P5 **与 P2b 六行全部完成**——P2b 卡在的那道 `INDEX_VERSION` 批准门于 **09-27** 通过（版本 2→3 时 `artist_words` / `album_words` 各带一路 `_tri` 进了 schema）。产物 `preview/audio.rs`、`preview/transport.rs`、`preview/soundtrack.rs`、`media/waveform.rs` 已在 `main` 上。**这一节没有待办了**，09-28 还顺手多给了两格：音频卡片可用封面或波形（设置里一颗开关），采样率/声道/位深/比特率有自己的索引面与五个限定词。

先说两条决定排序的事实：

- **音频已经在导入管线里，且没有空转。** `DecodeStage` / `VisualSigStage` 各自开头 `if io.kind != Image { return Ok(()) }`；`ThumbStage` 走 `thumb::ensure` 的音频臂，只在**有内嵌封面**或**包络已在缓存**时产出卡片，否则 `None` → kind 图标：跳过是显式契约，且导入路径不会为一张卡片起 ffmpeg。`ProbeStage` 根本没有音频分支，时长是 `MineStage` 用 lofty 挖的，所以音频**永远不会为探时长抢一个 ffmpeg 进程槽**。逐类型矩阵见 [IMPORT-PIPELINE.md](./IMPORT-PIPELINE.md)「每种类型实际经过哪些阶段」。
- **因此后续任何一步都不需要重新导入。** 时长已在 `assets.duration_ms`、标题已是资产 `title`、艺术家/专辑已在 `facts.media`；缩略图按内容哈希寻址且可懒生成，老资产下次缓存未命中即自动补。

| 阶段 | 做什么 | 为什么排在这 | 量级 |
|------|--------|--------------|------|
| ~~**P1**~~ **已完成（2026-09-23）** | 归类抽出单一 `is_audio_ext`（aiff/aif/aifc/oga 补入，「打开方式」改为委托它）；`wma` 补 mime；`mine_audio` 改走内容嗅探让 `.oga` 能读；`artist` / `album` 进检查器属性页（9 份 locale） | 实测：真实 `.oga` 走完 stage→commit→query，落 `kind=Audio mime=audio/ogg duration=Some(6128)` | — |
| ~~**P2a**~~ **已完成（2026-09-23）** | `thumb::ensure` / `regenerate` 加 `Audio` 臂，用已在依赖里的 lofty `Tag::pictures()`；优先 `CoverFront`，其余取最大者，**跳过 APIC 类型 1/2 的文件图标**（"取第一张"会选中 32×32 图标糊掉卡片）；复用 `guess_file_type` 让 `.oga` 与标签读取口径一致 | 零新依赖、零迁移；缩略图按内容哈希寻址可懒生成，老资产下次未命中即补 | — |
| ~~**P2b**~~ **已完成（2026-09-27）** | `artist` / `album` 进全文索引：`artist_words` / `album_words` 各带一路 `_tri`（`search.rs:543-544`），并各配一个限定词 | 曾长期卡在同一道 `INDEX_VERSION` 批准门上；09-27 涨到 3 时通过，代价（每个老库首次搜索前清建一次全文索引）由那一次一起付掉 | — |
| ~~**P3**~~ **已完成（2026-09-23）** | `preview/audio.rs` = `AudioPlayer`，直接复用 `AudioEngine`（它本就只吃一个文件路径：`AudioPipe::open(path, seek_ms, speed)`、`has_audio_track` 用 `-select_streams a:0`，都与视频无关）。**没有**新建 media 层管线，也没有打开 rodio 的 symphonia/claxon。顺带把视频与音频共用的控件（播放/暂停、速度菜单、音量弹层、滑块回写守卫）抽成 `preview/transport.rs`，video.rs 少 107 行 | 原计划把 `AudioPipe` 提到 `media/audio.rs`——实际不需要：它已经是文件中立的，搬家只会制造 churn。`preview/audio.rs` 让给播放器、引擎改名 `preview/soundtrack.rs` | — |
| ~~**P4**~~ **已完成（2026-09-24）** | 波形包络 `media/waveform.rs`：400 桶、0=静音、ffmpeg 出**单声道 300 Hz**（让 swresample 替你做低通），缓存 `<cache>/libraries/<slug>/waveforms/<sha[:2]>/<sha>.bin`，首次播放现算；预览面板上方一条波形带。采样率/声道/位深/比特率进 `facts.audio`（注意 lofty 的 bitrate 单位是 **kbps**）＋检查器合成一行；「重新挖掘元数据」给老资产回填，且**只**合并 `facts.audio`/`facts.media`（字体 `facts.font`），视觉签名、色板、AI 结果一概不动 | 剩下的口子只有一个：那条波形带是显示用的，**拖它不能 seek**，seek 仍走滑块（`transport.rs` 的时间线） | — |
| ~~**P5**~~ **已完成（2026-09-24）** | 无封面的音频用包络画一张 512×288 卡片（`waveform::bitmap` 与预览带共用，卡片/预览只是 `Style` 不同）。代价问题在分配上解决而不是在画质上妥协：`ensure`（导入热路径）只认**已经缓存**的包络，`regenerate`（用户点「重建缩略图」）才付 ffmpeg。顺带补了三个洞：重建计划原先只覆盖图片/字体/模型且**完全跳过链接资产**（于是清理删掉链接缩略图、重建又不补），`clean_orphans` 不知道有 `waveforms/`，存储页的"缓存"少算一项 | 卡片是" recognizable 的一堆波形"而不是专辑图，这是没有封面时的诚实答案；实测：`regenerate` 对真实音频产出 512×288 卡且 `ensure` 不起进程（ffmpeg 在场时跑） | — |

**建议不追**：音频进 AI 分析（Serpent 也不分析音频；Trove 侧没有 ASR 也没有音频嵌入，要先补的是推理而非接线）；`audio_proxy` 转码产物（Serpent 因浏览器不能直解才转，Trove 用 ffmpeg 任意 seek，加了反而是退化）。

---

## H. 明确不追的（Trove 领先区）

写下来是为了防止下次对比时误判成"要补的洞"。

| 领域 | Trove | Serpent |
|---|---|---|
| 点云 | PLY 一等资产 + 强度 / ASPRS 23 类别 + 按字段着色（移植 CloudCompare 15 套色带 + 自定义锚点，shader 内解算故流式点云一次写 buffer 就重绘）+ 逐资产记忆 | **完全没有点云**：`src/shared/media-formats.ts` 里没有 `.ply`。它只出现在研究文档 `docs/developer/research-media-format-support.md:46`，与 DAE / 3MF / USD 并列，措辞是"再排期"——本表上一版写的"P2 候选"给它加了个原文没有的优先级标签 |
| 大文件 | 4M 点常驻预算（`streaming_point_cloud.rs:26`，超预算**均匀抽稀**而非失败：`:20-25`）/ 每帧可见点预算 30 万、每帧从盘上取 5 万（`:14`、`:17`）/ 512 MiB 分块解码预算（`media/chunked.rs:55-59`）/ 页表常驻 256 MiB + LRU 逐出（`media/formats/virtual_memory.rs:18`）/ **覆盖保持抽稀** / 20 GB 不 OOM / meshlet：≥8192 面**且带顶点法线**才分块（`:33` 门槛、`:73` 的法线条件，`:27` 每簇上限 4096 面），逐帧视锥剔除（`preview/gpu3d.rs:876`、`:125-136`）/ `.trovecloud` 离线索引 v2：**每点 6 B 位置**（3×u16，块内归一，故精度随块大小走），带色 +3、带强度 +4、带类别 +1，合计 **6–14 B/点**；头 128 B、块表 40 B/块（`media/index/file.rs:8-19`、`:52-60`） | **只有警告**：`src/renderer/3d-viewer/limits.ts:6-8`、`:13` —— `MODEL_TRIANGLE_WARN_THRESHOLD = 2_000_000`（>200 万面弹提示）与贴图边长 >2K 的提示，注释原文就是 "v1 warns instead of downscaling"；无 LOD、无分页、无流式 |
| 无 GPU 环境 | CPU 软件光栅器，与 GPU 路径共享全部常量与构图 | 不可能（一切视觉都在 Chromium 里） |
| 视觉与语义检索 | dHash + 4096 桶直方图 + 向量索引（RRF 融合、multimodal 缩略图 data-URI）+ 拼音 | **Fts5 trigram 之外零命中**，产品简报明确排除向量与感知哈希 |
| 像素编辑 | 旋转/翻转/裁剪（百分比按各图尺寸解析）+ JPEG 质量 + **写回原文件** + 标题栏单图快捷编辑 | **完全不写像素**：查看器旋转镜像只是 CSS 变换，转码交给插件 |
| 格式转换 | 内置 5 目标 JPEG/PNG/WebP/BMP/TIFF + 长边限制 + 可回灌导入 | 交给插件 |
| XMP | 标准 sidecar 导出（原子写、全转义） | 明确不做，纯 DB |
| 重复查找 | dHash 分组 + 保留最新 + CLI | **仅导入时刻**：`src/renderer/ContentDuplicateDialog.tsx`，计划字段 `suspectedDuplicateCount` / `libraryDuplicateCount`（`src/automation/command-registry.ts:3731`）；库里没有独立的重复报告命令 |
| 存储统计 | 分目录 + 标出可重建项 | **无用户可见面板**（逐个组件与设置页核过，确实没有） |
| 屏幕截图 | 见 §G | 无 |
| HEIC / HEIF / AVIF 解码 | `probe.rs:345` `heif_to_image` 走外部 libheif（`heif-dec`），一处覆盖 `.heic .heif .avif` 三种后缀，带 `proc::slot()` 并发闸与超时 | Serpent **只有 AVIF**（sharp/libvips，工单 `Serpent-c93c75`），HEIC 仍在其研究文档的候选列表里没进注册表——它内部反而不一致：`image-sequence.ts` 已把 `.heic/.heif` 当序列帧后缀。**这项 Trove 覆盖面更广**，别在格式对齐时漏判 |
| CLI | **21 个顶级命令 / 29 个叶命令**（09-28 第二轮之前是 20 / 26：新增 `sequence`，下挂 `create` / `dissolve` / `fps` 三条。历史：上一版记 28，偏高两个。顶级以 `trove --help` 为准正好 21 条；`collection` 下 6 条、`index` 下 2 条、`sequence` 下 3 条，其余 18 条本身就是叶子。枚举定义：`cli.rs:59 Command`、`:158 SequenceCommand`、`:473 CollectionCommand`、`:518 IndexCommand`（09-28 第二轮加了 `SequenceCommand`，所以后两个的行号比上一版漂了 45 / 45 行——行号是这份文档里最短命的引用，改 CLI 时记得一起改这里）。旧写法：`libraries info paths list search get tags collections duplicates folders doctor import analyze set tag trash restore purge collection index`，第二轮末尾多了 `sequence`；`collection` 下 6 个子命令 `:473-515`，`index` 下 2 个 `:518-529`，`sequence` 下 3 个 `:158-192`），stdout 恒为一个 JSON、退出码 0/1/2/3、桌面版持库时**只读并发**（`main.rs:1-13`） | CLI **已撤回**：`docs/glossary.md:32` 记着 2026-07-28 移除了那层只读 CLI 基座，此后没有通用 CLI（机器面入口就是 MCP / 脚本 / 插件三条） |
| 分面计数（滤镜下拉里带数量） | **2026-09-28 新增**：`store/facets.rs`（476 行，含 4 条测试）算七个维度——kind / ext / tag / rating / favorite / usage_status / orientation——每个都是一条 `GROUP BY`，**复用列表自己那条 `WHERE`**（`build_where(..., WhereMode::Driving)`），所以"选项上的数"和"点了它之后的行数"不可能不一致。排名型列表（搜索结果、最近查看）另走 `compute_for_ranked`：把冻在 `BrowseSession` 里的那批 id 用**一个 `json_each` 参数**接在 WHERE 前面（沿用 R3 后半那个形状，不是两万个 `?`），所以计数只统计排名返回的那些资产。工具栏六颗（kind / tag / shape / rating / format + 各自的选项）都显示成 `PNG (42)`；分页**追加**时沿用上一轮的计数，不重算。**成本本轮没量**：一次全新列表要跑七条 `GROUP BY`（其中 tag 那条把整条 WHERE 包成子查询再 join 两跳），而 `docs/PERF-VS-SERPENT.md` 那份对照是在这之前测的——大库上换视图/改筛选时那七条的合计成本未知，值得在同一个 harness 上补一组数 | **无 facet 概念**：`grep -rn "facet\|Facet" reference/Serpent/src/` 零命中。它有的只是标签面板自己的 `GROUP BY tag_id` 计数（`library-service.ts:20949`、`:20972`），与当前筛选上下文无关，也不出现在任何筛选下拉里 |
| 标签层级链（一次输入建多级） | 同一颗输入框给两种落法：平级追加，或把 `a, b, c` 建成 `a → a/b → a/b/c`（`inspector.rs` 的 `append_tags_chained`，每级的父标签就是前一个名字，逐级 `create_tag(name, parent_id)`） | **无**：它的逗号输入只做"拆成待提交 chip"，全部平级挂到资产上 |
| 后台任务的线程占用 | 所有任务共用一个 `TaskPool`（`tasks/mod.rs:527`，`available_parallelism().max(2)` 个工作线程，线程名 `trove-pool-N`）。以前是**每个任务一条线程**，所以"同时开十个导入就起十条"这件事结构上不再可能。另加一层：每任务的进度事件合流成最新一条，`MAX_EVENTS_PER_JOB = 128` 从此只约束终态事件（`progress_events_are_lossy` 钉着） | 无对应物可对照：Electron 侧是主进程 + worker + 若干 utility process，任务不各占一线程。**这一条列在这里不是 Trove 领先**，是记下"本轮改了执行模型"，免得下次有人把 `TaskPool` 当成性能优化后的既成事实——它同时把"并发任务数"从隐式无界变成了显式有界 |
| 性能对照的可复现性 | **2026-09-28 新增** `docs/PERF-VS-SERPENT.md`（305 行）+ `bench/`（9 个入库源文件）：同一份 Serpent fixture（20k / 100k）、两边同题指标名一一对齐、`bash bench/run-all.sh` 可整轮重跑、`node bench/aggregate.mjs` 重画表格。结论是查询层 20 项配对指标 Trove 赢 18 项，输的两项（切合集 0.46× / 切文件夹 0.09×）都指向同一个原因（每次切视图都算精确总数，而那条 COUNT 挂着 20 万个标量子查询）；进程层是出窗 559 ms 对 1543 ms、常驻 PSS 258 MB 对 572 MB | 有自带基准（`tests/worker/comprehensive-perf-bench.test.ts` 等）且本轮**直接拿它当被测方跑通**，但它的摄取侧基准在本机 Linux 跑不起来（`bench/serpent/README.md` 记了两处环境绕行：`ENOBUFS` 与 sharp 在 `ELECTRON_RUN_AS_NODE` 下段错误），所以导入层只有 Trove 的绝对值，没有可信的对方数字 |
| 3D 贴图与材质 | ⚠️ **上一版写的"Trove 唯一实打实的落后"已于 2026-09-27 追平**。Serpent 侧对应不变：`src/renderer/3d-viewer/loader-registry.ts:35-36` 的 GLTFLoader + MTLLoader、伴生纹理发现、Phong→Standard 升级 | **已落地**：`Mesh` 新增 `texture: Option<Box<TextureData>>`（`media/formats/types.rs:271`），`TextureData` 含 UV 坐标 `uv: Vec<[f32; 2]>`、贴图槽 `slot: Vec<u16>`、金属粗糙度槽 `mr_slot: Vec<u16>`、材质因子 `factors: Vec<[f32; 2]>`、以及解码好的 RGBA8 贴图 `maps: Vec<TextureMap>`（`:143-169`）。glTF 加载器读取 `materials` / `TEXCOORD_0` / 纹理采样（`formats/gltf.rs` 全文 72 处 texture/TEXCOORD/material 引用）；OBJ 加载器读取 `vt` / `usemtl`（`formats/obj.rs` 26 处引用）。GPU 渲染侧 `gpu3d.wgsl` 新增 `@group(0) @binding(1) var model_textures: texture_2d_array<f32>`（`:62`）+ `model_sampler: sampler`（`:63`），以及完整的 `fs_model_textured` 片段着色器（`:347-396`）做 base-colour 采样与金属粗糙度贴图。Serpent 侧对应：`src/renderer/3d-viewer/loader-registry.ts:35-36` 的 GLTFLoader + MTLLoader、伴生纹理发现（`:5-6`、`:40`、`:109`、`:278`）、`missingTextures` 上报（`:87`、`:149`）、Phong→Standard 升级（`:29-30`） |

---

## I. 顺手记下的文档失真（本轮未改）

README 与 README.en 里的 8 处已在 09-23 那轮修正，`docs/` 下的 SHA-256 / pHash 共 19 处也已于 09-24 改完（见下表划掉那行）。以下是 `docs/` 下**仍在**的，改到相关模块时顺手带走；本轮（09-24）只核不改，核完的结论都附了行号：

| 文件 | 说法 | 实际 |
|---|---|---|
| `MEDIA-FORMATS.md` | ~~AIFF 归音频、波形、频谱、音频内嵌播放器~~ | **音频一节已改写为现状**（12 种归类、可听、波形带与波形卡片、逐字段落点）。本轮又修掉两处、还剩两处：**EPUB** 已改成"只有「打开方式」里有它，`probe` 不认"；**视频截帧入库**那一行随 T1 实现改成实话（并改名为「截当前帧入库」，说清是按源尺寸重解一帧而不是取屏幕上那块 720 宽缓冲）；**文档缩略图**打折——文本资产现在有自己的行做卡片，PDF / 归档仍是 kind 图标，并且文档里现在就这么写着；**动图逐帧/帧控制**（`:276-277` 那两行）仍未修 |
| `MEDIA-FORMATS.md:31` | AVIF 由 image crate 解码 | 走外部 `heif-dec`：`media/probe.rs:320`（尺寸路径的注释）与 `:345` `heif_to_image`，**一处覆盖 `.heic .heif .avif` 三种后缀**。`:31` 那句仍未改 |
| `AI-TAGGING.md` | `ai/chat.rs` / `ai/tagging.rs` / `tasks/autotag.rs` / `trove autotag` | 已被 `4515046` 重构删除，现为 `tasks/ai_analysis.rs` + `trove analyze`。**2026-09-24 再确认这些名字还在文档里**：`:27`、`:74`、`:120`、`:166`，以及 `:199-204` 整段 `trove autotag …` 示例。`ai/` 目录实际内容是 `analysis.rs · embedding_openai.rs · http.rs · mock.rs · mod.rs · search_planner.rs · vendor/`；`tasks/` 是 `ai_analysis.rs · embed.rs · ignore.rs · import.rs · mod.rs · watch.rs` |
| `TAG-COLLECTION.md` | `:70` 标签合并、`:118` 封面设置、`:173-182` 规则字段表（`is/is_not/contains/not_contains/starts_with/before/after/between` + `width`/`height`）、`:271` 声称 `collections.rs` 管封面 | 合并与封面无（见 §E 两行）；规则真实形状是 `{"op":"and\|or","children"}` / `{"op":"match","field"…}`，比较符只有 `eq/ne/gt/gte/lt/lte`，文档里的 `is/is_not/contains/before/between` 与 `width`/`height` 字段都不存在（`model/query.rs:117-129`：条件仅 `Ext\|Kind\|Path\|MinRating\|Favorite`） |
| `CONFIGURATION.md` | ~~键位表~~ **已于 2026-09-27 整段重写**：旧表 20 行里只有 7 行是真注册过的绑定，其余（`Shift+Delete` 永久删除、`F` 收藏、`1-5` 评分、`T` 标签、`Ctrl+F`、`Ctrl+C`、`F11`、`Ctrl+=`/`-`/`0`、`Ctrl+]`/`[`）全是编造的，且 JSON 示例用的 `workspace/open` 式键名根本不是真实格式（`keybindings` 的键是动作名本身）。新表逐条照抄 `keybindings.rs` 的 21 条并标出上下文。`:22`、`:41` 的 `~/Documents/Trove` 目录树**仍未核** | 与 `crates/trove-core/src/keybindings.rs:26-148` 的 **21 个可配动作**（09-27 起多一个 `QuickLook`）（MoveLeft/Right/Up/Down · OpenPreview · QuickLook · TrashSelected · SelectAll · ClearSelection · Undo · Redo · CopyImage · ImportFiles · OpenSettings · Screenshot · RefreshLibrary · BatchRename · BatchConvert · AutoTag · EnterVideoFullscreen · ExitVideoFullscreen；其中 **5 个默认值是空串**，即"未绑定但可绑"，`:105-107`）、**八个**内置设置页 + 插件贡献页（About / Appearance / Files / Model / Search / AI / Shortcuts / Plugins，注册在 `dialogs/settings/mod.rs:398-412`；**深链枚举只覆盖前 7 个**，不含 Plugins，`:85-120`）、XDG 四根目录均不一致 |
| ~~`PREVIEW-SYSTEM.md:91`、`MEDIA-FORMATS.md:94`~~ | ~~视频截帧入库~~ | **不再是失真**：2026-09-24 的 T1 把它实现了（`media/video.rs::write_frame_png` + 预览工具栏的相机按钮），两行文档也顺手改成说得准确——按源尺寸重解一帧，不是抓屏幕上那块 720 宽的缓冲 |
| ~~`PREVIEW-SYSTEM.md:88`、`MEDIA-FORMATS.md:92`~~ | ~~倍速播放 0.5× – 2×~~ | **已于 2026-09-24 改为实际值**：9 档、上限 4×，`preview/transport.rs:32` `SPEEDS: [f32; 9] = [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0]`，视频与音频共用（两处都写明了是这 9 档） |
| `MEDIA-FORMATS.md:291-292`、`PREVIEW-SYSTEM.md:18,49` | 动图"逐帧播放 / 帧控制：暂停/逐帧/速度" | 播放为真、**控制为无**：GIF 与动态 WebP 由 gpui 原生解，APNG 走 `panels::common` 的多帧解码缓存，`preview/image.rs` 只有"有 animated 源就播，否则退回缩略图"两个分支，没有任何逐帧/暂停/倍速入口（09-25 再确认：`next_frame` / `FrameStep` 这类符号在 `preview/image.rs` 里零命中）。**这一行上轮引的 `:276-277` 已经漂到 `:291-292`**——同一个文件在同一轮里被改过 146 行，行号是这份文档里最短命的引用 |
| ~~`crates/trove-core/src/store/schema.rs:22-27`~~ | ~~模块注释说 `UPGRADES` "holds six steps"~~ | **不再是失真**：2026-09-28 的 v20→v21（`task_journal`）那一步落地时，注释同步改成 "holds seven steps" 并列出了七段（`schema.rs:22-28`、`UPGRADES` `:62-105`）。"v14 之前的库打不开"这个结论仍然成立 |
| `SEARCH-INDEXING.md:104` | 9×8 差异哈希 | **这条是对的**；错的是 `media/search.rs:150` 的模块头注释写成 "DCT-based pHash" |
| ~~SHA-256 / pHash 散落在 `docs/`~~ | ~~6 个文件、19 处把内容哈希写成 SHA-256、把视觉指纹写成 pHash~~ | **已于 2026-09-24 全部改为 BLAKE3 / dHash**（逐处核对：`media/hash.rs:3` "Every content hash in the pipeline is BLAKE3"、`ai/mod.rs:104` `source_hash` 亦然、`media/search.rs:163` 调 `Self::dhash`）。仍待修的是代码注释 `media/search.rs:150` |
| `settings.search_mode_visual`、`settings.sig_coverage_desc` | 「视觉（pHash + 颜色）」「带 pHash + 颜色指纹的图片数」 | 界面上说的还是 pHash，9 份 locale 各 2 条。文档已经改完，这 18 条字符串是剩下的一致性缺口——改文案要过一遍全部语言，所以留在这里没有顺手做 |
| `store/task_journal.rs:5-8` | 模块注释："On library open, [`load_interrupted`] reads tasks that were running or paused when the process died, **so the UI can offer to resume or retry them**" | **写侧为真、读侧为假**：`load_interrupted` 定义了、能编译、函数体正确，但 `grep -rn load_interrupted crates/` 除了它自己和这句注释**零命中**，开库路径上没有调用方。注释里那句"so the UI can offer to resume"描述的是一个还不存在的界面。**本轮新增的注释性失真共三处，这是第一处**，形状和 §B「备份可还原」那格（写侧齐、读侧为零）完全一样。**已于 2026-09-28 第二轮改掉**：`load_interrupted` 现在由 `Library::assemble` 在开库时调用，面板有一块独立的"上次中断"区；注释也重写成实话——**读得到、但不提供重试**，因为 journal 从不记录启动一个任务所需的输入 |
| `plugins.rs:180-181` | `pub fn task_kinds(disabled)` 的文档注释："The UI uses this to populate **the task panel's filter and the settings page's plugin list**" | 两个都不成立：`grep -rn "plugins::task_kinds\|task_kinds(" crates/trove-app/` 零命中，任务面板没有按 kind 过滤的控件，设置页的插件列表也不问这个函数。`Plugin::task_kinds()` 这个钩子**本身是对的**（`TaskKind::Custom` 与 journal 的 `plugin:<name>` 前缀能原样往返，`task_journal.rs:139`、`:156-163`），缺的是调用方，以及 `builtin::SidecarNotes`（`plugins/builtin.rs:128`）没声明任何一个 kind，所以今天返回的永远是空表。**已于 2026-09-28 第二轮改掉**：调用方是 `Library::assemble`（`plugins::task_kinds(&config.disabled_plugins)` → `TaskManager::declare_task_kinds`），而且这道声明有牙——没声明过的 `TaskKind::Custom` 被 `StartError::UndeclaredKind` 拒。注释同时改成"喂给任务管理器"。**唯一使用者目前还是一个测试插件**：`SidecarNotes` 只有一个管线 stage 和一颗命令，没有该在后台跑的活，硬给它一个 kind 是假需求 |
| `crates/trove-app/locales/*.toml`（全 9 份） | `inspector.replace_tags_hint`、`inspector.replace_tags_failed` 两条 | **2026-09-28 起是孤儿键**：`replace_tags_from_input` 那个函数本轮被 `append_tags_flat` / `append_tags_chained` 取代，`grep -rn "replace_tags_hint\|replace_tags_failed" crates/ --include=*.rs` 零命中。留着不影响渲染（没人查它），但它让 `en.toml` 的叶子键数（现 **784**）虚高两格，而且 `app/i18n.rs:106-115` 那个棘轮表把九份 catalog 都算上了这两个键，删的时候要九份一起删，否则 de/es/… 的"多 N 个过期键" allowance 会跟着变。**顺手能收的一格 S** |
| `docs/PERF-VS-SERPENT.md:58`、`:197` | "Trove 构建：`cargo build --release`（`strip = true`，**`lto = "thin"`**）"；产物体积那行同样写"thin LTO" | **对本轮那些读数仍然正确**（它们确实是在 thin LTO 下测的），但对下一次重跑不再成立：`Cargo.toml:4-8` 现在是 **`lto = true` + `codegen-units = 1`**（fat LTO）。`bash bench/run-all.sh` 重跑出来的数字会和这两行描述不一致，也就和本文所有表格的来源描述不一致。**改法有两种，别默认第一种**：要么在重跑前先记下"读数对应 thin LTO 那版"，要么重测一遍并把 58/197 两行改成 fat LTO（后者要让 release 全量重编，本轮没做也没量） |
| `store/schema.rs:99-104`、`store/task_journal.rs` | v20→v21 迁移 + 185 行的 journal 读写 | **两处测试空白**：`store/mod.rs` 为以前的每一段迁移都有一条具名测试（`a_v14_library_is_upgraded_in_place:478`、`a_v15_library_folds_its_accent_and_survives_a_replay:530`、`a_v17_library_gains_the_ordered_indexes_and_their_statistics:602`、`a_v19_library_gains_the_indexed_source_path:665`），**v20→v21 仍没有对应的那一条**（09-28 第二轮补了 journal 的读写测试，没补这段迁移）；~~`task_journal.rs` 全文 `#[test]` 零条~~ **已补 3 条**：running 能被读回且落终态后消失、retry 累加在同一行不分叉、`plugin:<name>` 原样往返且未知 slug 照常报出，而它对外的每一句写库都是 `let _ = task_journal::record_*`（`tasks/mod.rs` 里 9 处，`:757`、`:831`、`:917`、`:959`、`:978`、`:1002`、`:1021`、`:1049`、`:1069`）——**写失败静默，且没有任何测试钉住"任务确实落库了"这件事**。§F「回收站保留期」那行批评 `library.rs:1916/1919` 用 `let _ = std::fs::remove_file` 把错误丢掉，理由是"被占用的文件静默留在原地"；journal 这里是同一个形状，只是代价还没人验过 |
| — | `store/schema.rs:10-25` | 迁移链在 0.5 前被删，**v14 之前的库无法打开**：这是决策不是 TODO，但值得在文档里对使用者显式声明 |
