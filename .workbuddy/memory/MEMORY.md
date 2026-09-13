# trove 项目长期记忆

> 详细方法论/实测数字在 skill 里（`trove-build`、`trove-search-benchmark`、`trove-ply-benchmark`、
> `split-dirty-tree-into-commits`），本文件只留**不变量**与「下一步会踩的坑」。

## 提交与工作区
- Conventional Commits（英文），按功能拆粒度。`.workbuddy/memory/*.md` 被 git 追踪；`.zcode/plans/*.md` 不提交。
- 推送绕行 SSH（执行器以 root 运行、ssh 配置不可读）：`GIT_SSH_COMMAND="ssh -F /home/noke/.ssh/config -i /home/noke/.ssh/id_ed25519 -o IdentitiesOnly=yes -o UserKnownHostsFile=/home/noke/.ssh/known_hosts" git push origin main`
- ⚠️ 跑测试/基准必须 `TMPDIR=/home/noke/Code/trove/target/tmp`（/tmp 只有 10 MB tmpfs，否则几十个测试假失败）。
- ⚠️ trove-app 链接期吃内存，用 `cargo rustc -p trove-app -- -C link-arg=-Wl,--threads=1`；测试目标要把链接参数放进 `RUSTFLAGS`（`--` 后面的参数给了 libtest，rustc 收不到）。
- 按 hunk 暂存脚本 `/home/noke/.cache/trove-split/stage_hunks.py`（spec JSON，内部固定 `-U3`）。⚠️ `git stash push --keep-index --include-untracked` 遇被占用文件会「只存不清理且返回非零」，`&&` 链断后 pop 不执行、再 drop 就丢文件；验证中间提交用不带 `--include-untracked` 的版本。

## i18n（rust_i18n）
- `crates/trove-app/locales/{en,zh-CN}.toml` **必须同键同序**；插值 `%{var}`。未写进 toml 的 `t!("a.b")` 会原样显示 key。校验脚本见 `trove-build`。
- `shortcuts.actions.<ActionId>` + `settings.rs::action_label` 是快捷键显示名的两处来源，新增可绑定 action 时都要加。

## gpui / gpui-kit 实测坑
- **sprite atlas 永不淘汰**（只插不删、无 LRU）：任何「每帧新建 `RenderImage`」都会泄漏显存，换帧必须 `window.drop_image(old)`；凡「把自建帧塞进 img」的地方，退出/被替换路径也要归还（如 `viewport3d.rs::release`）。只能在 render 里做，因为只有那里拿得到 `&mut Window`。
- `Context::spawn` 闭包签名 `(WeakEntity<T>, &mut AsyncApp)`；`WeakEntity::update(cx, …)` 直接传 `cx`。`background_executor().spawn(async move {…})` 会把变量 move 进 future（外层还要用就先 clone）。
- `SliderState::set_value(value, &mut Window, cx)` 三参 —— 程序化同步滑块值只能在 render 里做。
- `img()` 接受 `ImageSource::Render(Arc<RenderImage>)`；BGRA 字节走 `image::RgbaImage::from_raw` + `image::Frame::new`。
- `context_menu(...)` 返回 `ContextMenu<E>` 只转发 `child`：`on_drag`/`drag_over`/`on_drop` 必须挂在它**之前**（否则 E0599）。
- `on_prepaint`（`gpui_kit::base::ElementExt`）只挂在 `Div` 上，必须在 `.id()` **之前**调用（之后变 `Stateful<Div>`，方法消失）。
- `CursorStyle` 手型是 `ClosedHand`/`OpenHand`（无 `Grabbing`）；`ScrollDelta::{Lines,Pixels}`（后者 `.as_f32()`）；双击次数用 `ClickEvent.click_count()`。
- `trove_core::media::mesh::Bounds` 会遮住 gpui 的 `Bounds`，同文件写 `use …::Bounds as MeshBounds`。
- 面板模块里加测试**不要 `use super::*`**：gpui prelude 会带进 `test` 属性宏 → `recursion limit reached while expanding #[test]`。显式列名。
- `panel!` 宏（`panels/mod.rs`）原本硬编码 `focus_handle` + `controller`，已扩出第三段可选字段：`panel!(Name, title, field: Ty)`。宏参数里**不能用 `///`**（会变 `#[doc]` token 打乱 `ident: ty`），用 `//`。目前只有 `FoldersPanel` 走宏，其余面板手写（可整理，未做）。
- `Library`（含 rusqlite `Connection`）**不是 Send**：DB 访问全在主线程；后台只做纯文件 IO/计算，回主线程提交。

## 查询与面板性能（不变量）
- `AssetQuery` **没有 `text` 字段**（故意删，防第二条 LIKE 路径）。自由文本 → `TextIndex::search` 出 id 名次 → `assets::rank_intersect` 求交；`build_where` 只管结构化条件。
- **rank 路径必须由候选 id 列表驱动**：`assets::WhereMode::{Driving, Rejecting}` —— 列表页 `Driving`，`rank_intersect` 用 `Rejecting`（给每个可索引条件前缀一元 `+`，文档化 no-op，语义不变）。不加会被 planner 挑 `idx_assets_trashed` 拖成全扫。测试 `ranked_where_clause_leaves_the_id_list_driving`，⚠️ 测试里 id 数要**上百**（SQLite 按 IN 长度估价，2 个 id 时反而选 id 索引）。
- `id_list(col, ids, first)` 两个静默 bug（已修）：① 编号必须从 `args.len()+1` 起，否则与前面条件撞号（`Got 2, needed 1`）；② 空列表渲染 `IN (NULL)` 而非 `IN ()`（tag 被删但仍在查询条件里 → `subtree_ids` 空）。`build_where` 末尾有 `debug_assert_eq!(highest_placeholder(&conds), args.len())` 兜底。
- **`render` 里的库读是最大单项开销**（比搜索路径还大）：`source_folders` 46–54 ms、`count_assets`×30 58–68 ms，原本**每帧**都跑。通用修法：按 `controller.generation` 缓存（`ExplorerPanel::snapshot_cache` 是模板）。**`active_*` 选中状态不要缓存**。⚠️ 缓存写法不能让 `match &self.f` 的绑定值活过 match（E0502）—— 先判 stale 并写入，再单独 `match` 取引用。
- `counts_by_tag`（一条递归 CTE 出全部 tag 计数）**只快 ~10%**，价值在去掉 N+1 往返，不是数量级优化。
- 搜索侧：`search_queue` outbox 由触发器填充，`search::drain` 消费。改 schema 或查询语义**必须 +1 `search.rs::INDEX_VERSION`**（存量库启动时全量重建，`Library::open` 同步跑在 UI 线程）。`drain` 两条铁律：先 `index.commit()` 再删队列行；整批用一条 `DELETE … WHERE rowid IN (…)` 包在 `unchecked_transaction` 里。`DRAIN_BATCH = 8000` 别调小。
- `asset_fts` 已彻底不存在（v2/v3/v11 的 CREATE 已删，只留 DROP）；`schema_has_no_fts5_leftovers` 钉住。⚠️ 判断迁移是否「已发布」看 HEAD 的 `SCHEMA_VERSION`，别凭文件内容猜。

## 三维模型 / PLY（不变量）
- `AssetKind::Model`；OBJ/STL/PLY → `media::mesh`，缩略图 `media::render3d`（CPU 光栅化），交互视口 `library/gpu3d.rs`（自建 wgpu + 离屏渲染 + readback）。
- **CPU/GPU 必须共用 `render3d::{Framing, VertexData, PointData}`**；逐点颜色的唯一决定点是 `render3d::base_color`。点云由 `Mesh::is_point_cloud()` 分流。
- **`PointData` 步长 9 个 f32**：改它要同时改 `PointData::STRIDE`、`gpu3d.rs` 的 point 管线 `vertex_attr_array!`、`gpu3d.wgsl::vs_point`（`the_vertex_inputs_match_the_buffer_layouts` 校验）。uniform 共 9 成员、`UNIFORM_SIZE = 192`。wgpu 29 与 gpui 内置版对齐（单版本单后端）：`immediate_size: 0`、`bind_group_layouts: &[Option<&BindGroupLayout>]`。
- **WGSL 无 GPU 也能校验**：`naga` 作 dev-dep，测试里 parse + validate + `Layouter` 断言 uniform offset。改 shader/uniform 后跑 `cargo test -p trove-app`。
- PLY 能力边界与性能基线见 `trove-ply-benchmark`／`docs/ply-load-bench.html`（⚠️ 报告与脚本未提交）。

## 主题与外观（gpui-kit 的 Theme）
- `Theme` global（light/dark 两个 `ThemeConfig` + 当前 `ThemeMode`）+ `ThemeRegistry` 按名存。切换 = 换 config + `Theme::change(mode, window, cx)` + `cx.refresh_windows()`。入口唯一：`app/theme.rs::apply_from_settings`。
- `build.rs` 用 `cargo metadata` 定位 gpui-kit 的 `themes/`，拷进 `OUT_DIR/themes` 生成 `builtin_themes.rs`（36 个主题 = 25 dark / 11 light；框架只注册 Default Light/Dark）。`TROVE_THEMES_DIR` 可覆盖。用户主题在 `<config>/trove/themes/*.json`（覆盖同名内置）。
- ⚠️ `apply_from_settings` 在 appearance 观察者里必须传 `Some(window)`：Linux 上 `cx.window_appearance()` 的 RefCell 正被借用，直接查会 panic。

## Open With —— ⚠️ 已在 19a3dd8 被整体删除
原 `services/open_with.rs`（扫 `$XDG_DATA_HOME`/`$XDG_DATA_DIRS` 下 `.desktop`、展开 `Exec` 字段码）与 app 侧 `library/open_with.rs`（`Origin::Stored` 先复制到 `<config>/edit/<asset_id>/` 再交给外部程序，因为内容寻址 blob 不能就地编辑；`edited_copy()` 先比大小、≤32 MiB 才 hash）**在 2026-09-13 的大重构里被删除**，全代码无调用者（`media/probe.rs` 里的 `open_with_defaults` 是 jxl_oxide 的无关 API）。若以后要恢复，从 `19a3dd8^` 取回；「blob 不能就地编辑」这条约束仍然成立。

## 工具栏每帧库读（2026-09-13）
- `WorkspacePanel::toolbar_row` 是 `&mut self`、每帧重建，曾把 `assets::distinct_exts`（~25 ms/次）放在里面。
- `distinct_exts` 的 `LOWER(ext)` 包住索引列 → `idx_assets_ext` 失效 → `SCAN assets` + temp B-tree。改成 `SELECT DISTINCT ext … ORDER BY ext`，case folding/去重挪到 Rust（`to_ascii_lowercase`，对齐 SQLite 的 ASCII-only `LOWER`）：12 种扩展名/10 万行 24.6 → 15.2 ms。
- `WorkspacePanel::filter_exts` 按 generation 缓存，`format_filter` 改吃 `&[String]`。
- 量过但**不值得动**：`tags::list` 0.05 ms、`by_ids × 20` 0.3 ms、`AppConfig::load()`（每帧读 JSON 配置文件）**仅 0.012 ms**。
- 待拍板的迁移候选：`CREATE INDEX … ON assets(ext) WHERE trashed_at IS NULL` → scratch 表实测 24.6 → **3.96 ms**（变 index-only）。缓存解决「每帧」，这条解决「导入后的首次读」。
- ⚠️ 别在退化的单一基数数据集上量索引查询（本库压测数据全是 `ext='jpg'`，把收益放大成假的 8000×）；别在单测里断言索引计划（小表启发式与满规模相反）；`plan_of` 要在计时旁边抓，否则建完索引后三行计划会全显示新索引。

## 面板内元素与标题栏 action 对齐（左 dock / explorer）
- dock tab 栏把 `title_suffix` 包在 `px_2` 盒子里、后跟一个 `gap_1` 才到 toolbar 槽 → 标题栏按钮右缘距面板右缘 **12px**；面板内容自己 `p_1`(4) + 行 `px_2`(8) 也是 12px，故行内数字天然对齐，分组标题（如智能收藏夹的 `+`）只有 4px，需补 `pr(px(8.))`。常量在 `panels/explorer.rs` 顶部「Layout metrics」（12 是推算值，未 GUI 复核）。
- 嵌套缩进用 **padding**（`.pl`）不要用 margin：行是 `w_full`，margin 会把右侧计数挤出面板。

## 外部工具集成
- ffmpeg / ffprobe 是**可选运行时依赖**（不链接、只执行），缺失时降级：视频缩略图退回图标、预览退回静态海报、探测退回 `media::probe::video_facts`（只读 MP4 moov，无帧率）。
- `services/screenshot.rs` 纯函数 `plan_for(mode, custom, dest, Platform)` 可单测（Platform 显式注入，避免测试里改 env）。

## 依赖版本约束（2026-09-13 去重后，别乱动）
详见 `trove-build` 的依赖审计表。硬约束：`resvg` 钉 ^0.46、`tantivy` = 0.26（升它必须 +1 `INDEX_VERSION`）、`sha2` 留 0.10、`jieba-rs` 留 0.7、`image` 特性只在 workspace 声明一处；`thiserror` 1/2 双份是已知可接受。统计重复必须按 host 平台（`cargo metadata --filter-platform x86_64-unknown-linux-gnu`），别用 `cargo tree -d`。
