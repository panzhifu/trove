# trove 项目长期记忆

> 方法论/实测数字在 skill（`trove-build`、`trove-search-benchmark`、`trove-ply-benchmark`），
> 逐日细节在 `.workbuddy/memory/YYYY-MM-DD.md`；本文件只留**不变量**与坑。

## 构建 / 环境（硬约束）
- 测试/基准必须 `TMPDIR=/home/noke/Code/trove/target/tmp`（/tmp 仅 10 MB tmpfs，否则几十个测试假失败）。
- trove-app 链接吃内存：`RUSTFLAGS="-C link-arg=-Wl,--threads=1"`（`--` 后的参数归 libtest，要放 RUSTFLAGS）。
- 推送绕行 SSH（执行器以 root 运行）：
  `GIT_SSH_COMMAND="ssh -F /home/noke/.ssh/config -i /home/noke/.ssh/id_ed25519 -o IdentitiesOnly=yes -o UserKnownHostsFile=/home/noke/.ssh/known_hosts" git push origin main`
- clippy 基线生产代码 **0 告警**；测试基线 trove-core 322 + trove-app 20（跳过 2 个真机 GPU）。
- 依赖硬约束：`resvg` ^0.46、`tantivy` = 0.26（升级必须 +1 `INDEX_VERSION`）、`image` 特性只在 workspace 声明一处。
- Conventional Commits（英文），按功能拆粒度；`.workbuddy/memory/*.md` 被 git 追踪。

## 环境限制（性能结论可信度）
- 执行器与宿主隔离（独立 PID namespace、无 systemctl/D-Bus/GUI）→ 滚动等 GUI 测量只能用户自己采。
- swap 曾被吃到 94.6% → 内存回落前基准数字不可信（早期导入基准就是垃圾数据）。
- 目标盘 btrfs 已用 81% 且碎片化，并发小文件写争元数据锁。
- ⚠️ 基准方法论：**同坐次内 A/B**（跨坐次系统负载漂移，第二轮整体慢 30%+）；**计时边界要把准备工作移出 `Instant::now()`**；批量事务 vs autocommit 差 100×（4.49→0.048 ms/行），不公平对比会得出 7.8 ms/文件这种假结论。

## 线程模型
- `Library`（rusqlite `Connection`）不是 Send：DB 全在主线程，后台只做纯 IO/计算回主线程提交。
- `Context::spawn` 闭包 `(WeakEntity<T>, &mut AsyncApp)`；`background_executor().spawn` 会 move 变量，外层还要用就先 clone。

## 滚动 / 每帧渲染
- gpui list 内核（`gpui-pre-0.3.3/src/elements/list.rs`）：判定好（B+ 树 O(log n) 偏移、400px 头部 overdraw、delta 合并），重建差 —— **可视行每帧无条件跑 build 闭包**（list.rs:1074），无元素缓存（ListState 只存 SumTree 高度），宽度变化全量作废行高缓存（list.rs:1543-1558，trove 靠 150ms debounce 缓解）。想跨帧复用只能自己包 `Entity`。
- 🔴 **gpui `loading_assets` 是无上限 FxHashMap**（`gpui/src/app.rs:548`），只有显式 `remove_asset` 才淘汰；trove 从不对网格缩略图调它 → CPU 侧解码结果永久驻留（真泄漏，加重 swap 压力；区别于 sprite atlas 管 GPU 纹理那条）。
- 🟢 **每帧热点已修（2026-09-14，`perf(workspace)` 提交）**，四条都靠「按 `controller.generation` 缓存 / 惰性化」：
  - `title_label` → `WorkspacePanel.title_cache: Option<(u64, String)>`（dock 的 `title` 在面板 render **之外**跑，原来每帧两跳 SQLite）。函数签名已是 `&mut self`。
  - `visible_assets` → 只在 rows 真的重建时发布（局部 `layout_changed` 标志），读取走 accessor `visible_assets()`；字段 `pub(crate)`。
  - 每帧快照 clone → `controller.selected_assets` 与 `VisualSearchResults.ids` 都是 `Rc<Vec<Uuid>>`，`DataKey/ViewKey.visual` 是 `Option<Rc<Vec<Uuid>>>`（两个 key 每帧重建，原写法每帧深拷至多 CANDIDATE_CAP 个 id）。变更一律 `Rc::make_mut`。
  - `page_guard` → 面板字段 `Rc<Cell<usize>>`，记「上次请求是哪个 `grid_loaded`」，同一 cursor 只请求一次；视图切换置 `usize::MAX`。
  - ⚠️ 改 `Rc<Vec<_>>` 字段的注意点：`.clear()` 报 `E0596`，要整值替换；`mem::take(&mut f)` 要写 `Rc::make_mut(&mut f)`；**`action_targets()` 契约返回 `Vec<Uuid>`**，得写 `(*self.selected_assets).clone()`。
- 探针方案已放弃（2026-09-14 用户要求移除，代码全删无残留）；**滚动性能至今未实测**——上面四条是「把每帧 O(n)/SQL 挪出热路径」的确定性收益，但没有帧时间数字；要量化只能用外部 profiler 或对比 digiKam（Eagle 未装且闭源）。
- trove-app 无 examples 目录；现有 bench（layout/relayout）都是纯 core。

## 导入管线（结论已收敛）
- 四层：`media/import.rs`（stage_all 并行）→ `tasks/import.rs::run`（COMMIT_BATCH=16 分块事务）→ `jobs.rs`（80ms 轮询）。旁路：`import_files`（media package）、`services/collect.rs`（HTTP inbox）。
- 🟢 P0-1 已修（2026-09-14）：`metadata::mine(path, kind, color_source)` 的 `color::dominant_colors` 改吃缩略图，图片不再二次解码 —— 大图 mine 146 → **3.27 ms/文件（-98%）**。
- 🟢 `add_asset` **无成本**（~0.14 ms/文件，批量事务下 A/B 两轮无差异）—— 「7.8 ms」是 autocommit fsync 税的误判，已结案。
- 🟡 staging 并行加速比与文件大小强相关：小图 1.31× / 中 2.47× / 大 4.21×（4 线程）。小文件批次瓶颈在 FS 元数据锁。
- 🟢 `stage_pool()` 注释里被推翻的「四线程最优、满池慢 26%」**已删（2026-09-14）**；现注释只说明它是固定上限，且加速比强相关于文件大小。
- 🟢 import.rs 注释引用不存在的 `compute_visual_signature_background()` 已于 2026-09-14 一并清掉。
- 🔴 `commit_staged` dedup 分支 `set_rel_path` 缺空串守卫（import.rs:330-332）：linked 导入 `rel_path=''` → 资产指向库根目录失效。未证实有真实触发路径，守卫该加。
- 🔴 重复导入无短路：第二次 stage 仍 0.45–0.52× 时间（mine + VisualSignature 照跑，结果被 dedup 丢弃）。
- `expand_dirs` 在 UI 线程同步跑（jobs.rs:178）；视频缩略图走 ffmpeg 同步子进程（单文件最贵步）。
- 未提交基准 example：`import_bench.rs` + `import_profile.rs`（clippy 各有 1 条告警，生产 0）。

## 查询与面板（不变量）
- `AssetQuery` 无 `text` 字段（故意删）。自由文本 → `TextIndex::search` 出 id 名次 → `assets::rank_intersect` 求交。
- rank 路径必须候选 id 驱动：`WhereMode::{Driving, Rejecting}`（列表页 Driving，rank_intersect 用 Rejecting，否则 planner 挑 `idx_assets_trashed` 全扫）。测试 `ranked_where_clause_leaves_the_id_list_driving`，⚠️ id 数要上百（SQLite 按 IN 长度估价）。
- `id_list` 两个静默 bug 已修：占位符从 `args.len()+1` 起；空列表渲染 `IN (NULL)`。`build_where` 有 `debug_assert_eq!(highest_placeholder, args.len())` 兜底。
- `render` 里的库读曾每帧跑（`source_folders` 46-54ms、`count_assets`×30 58-68ms）：按 `controller.generation` 缓存（`ExplorerPanel::snapshot_cache` 是模板），**`active_*` 选中状态不要缓存**。⚠️ 缓存写法别让 `match &self.f` 绑定活过 match（E0502）。
- `distinct_exts`：`LOWER(ext)` 包索引列致全扫，改 `SELECT DISTINCT ext ORDER BY ext` + Rust 侧 folding（24.6→15.2 ms，12 ext/10万行）。候选迁移未做：partial index `ON assets(ext) WHERE trashed_at IS NULL` → 3.96 ms。
- 搜索：改 schema/查询语义必须 +1 `search.rs::INDEX_VERSION`（存量库启动全量重建，同步跑 UI 线程）。`drain` 铁律：先 `index.commit()` 再删队列行；整批一条 DELETE 包 `unchecked_transaction`。`DRAIN_BATCH=8000` 别调小。`asset_fts` 已彻底删除并被测试钉住。
- ⚠️ 别在单一基数数据集上量索引（压测库全是 ext='jpg'）；别在单测断言索引计划；`plan_of` 在计时旁抓。

## 三维模型 / PLY（硬约束）
- CPU/GPU 共用 `render3d::{Framing, VertexData, PointData}`；逐点颜色唯一决定点 `render3d::base_color`；点云由 `Mesh::is_point_cloud()` 分流。
- `PointData` 步长 9 f32：改它要同步改 `STRIDE`、`gpu3d.rs` `vertex_attr_array!`、`gpu3d.wgsl::vs_point`（有校验测试）。uniform 9 成员、UNIFORM_SIZE=192。wgpu 29 对齐 gpui 内置版（`immediate_size: 0`、`bind_group_layouts: &[Option<_>]`）。
- WGSL 无 GPU 可校验：naga dev-dep，parse + validate + Layouter 断言 offset；改 shader 后跑 `cargo test -p trove-app`。
- 索引只读不建：侧车 `<file>.trovecloud` 在（离线 `index_build` 生成）才走 `IndexedCloud`，否则流式。产品永不建索引。转视锥只能 `Framing::frustum()`（列主序 + 深度 0..1，平面提取要转置；近平面 z=0）。批量点云 `insert_points`（一次 rebuild + 循环 decimate），逐点插入每万点重建整树。
- 流式加载别每块全量空间查询：每文件只精化 `STREAM_REFINEMENTS=32` 次（否则 40M 点 3.8s→69.5s）。
- GPU 后处理：EDL+补洞在 `gpu3d.wgsl::fs_edl`，开关 `AppConfig.point_enhance`（默认开，每帧重读）。
- meshlet：大网格（法线+≥8192 面）Morton 聚簇 4096 面逐簇剔除。`load_mesh` 仅 `.ply`>512MiB 走分块器（>2GiB 报错），别把 OBJ/STL 送进去。
- `max_points` 是采样预算不是「最近 N 点」：LOD 与选块都要铺满可见区，按距离取前 N 会退化成相机前一小块。
- PLY 基线见 skill `trove-ply-benchmark` / `docs/ply-load-bench.html`（未提交）。

## i18n / 主题 / gpui 坑
- locales `{en,zh-CN}.toml` 同键同序，插值 `%{var}`；快捷键显示名两处来源（`shortcuts.actions.*` + `settings.rs::action_label`）。
- 主题：入口唯一 `app/theme.rs::apply_from_settings`，appearance 观察者里必须传 `Some(window)`（Linux 上 RefCell 借用 panic）。内置 36 主题由 build.rs 从 gpui-kit 拷贝生成；用户主题 `<config>/trove/themes/*.json`。
- sprite atlas 永不淘汰：换帧必须 `window.drop_image(old)`；自建帧退出路径也要归还（如 `viewport3d.rs::release`）。
- `SliderState::set_value(value, &Window, cx)` 三参，程序化同步只能在 render 里做。
- `context_menu(...)` 只转发 child：`on_drag`/`on_drop` 必须挂它之前（E0599）；`on_prepaint` 挂 `.id()` 之前。
- `trove_core::media::mesh::Bounds` 遮蔽 gpui 的，同文件用 `use …::Bounds as MeshBounds`。
- 面板模块测试别 `use super::*`（gpui prelude 带进 test 宏 → recursion limit），显式列名。
- `panel!` 宏第三段可选字段已扩；宏参数里用 `//` 别用 `///`。只有 FoldersPanel 走宏。

## 历史 / 已修正
- Open With 删的是 `.desktop` 扫描 + 复制到 `<config>/edit/` 部分；`services/open_external.rs` 仍在用（`context_menu.rs:520` → `Library::open_in_external`）。
- ffmpeg/ffprobe 是可选运行时依赖（只执行不链接），缺失降级：视频图标缩略图、静态海报预览、moov-only 探测。
- scroll 探针（perf_probe.rs/SCROLL-PROBE.md/gen-bench-images.sh）已于 2026-09-14 全部删除。
