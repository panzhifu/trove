# trove 项目长期记忆

> 方法论/实测数字在 skill（`trove-build`、`trove-search-benchmark`、`trove-ply-benchmark`），
> 逐日细节在 `.workbuddy/memory/YYYY-MM-DD.md`；本文件只留**不变量**与坑。

## 参考代码目录（2026-09-15 建）
- `reference/` 放第三方参考仓库：`CloudCompare/`（稀疏浅克隆，只取 `libs/qCC_glWindow`+`libs/qCC_db`+`cmake`）、`gpui-kit/`、`fontmatrix/`、`snippets/`。已在 `.gitignore` 忽略，不进仓库、不参与构建。索引见 `reference/README.md`。
- 拉取/更新一律走 ssh：`GIT_SSH_COMMAND="ssh -F /dev/null -i /home/noke/.ssh/id_ed25519 -o IdentitiesOnly=yes -o UserKnownHostsFile=/home/noke/.ssh/known_hosts"`（绕过 777 的 `ssh_config.d` 软链）；https 走代理 502，但 `curl` 直连 raw/codeload 可用。
- CloudCompare 三维轴定论：角落那个是 `ccGLWindowInterface::drawTrihedron()`（像素正交空间 + 借 `viewMat` 旋转 + 清深度后开深度测试 + display list 缓存），场景内是 `ccCoordinateSystem` 实体；**没有 view cube**。解析与摘录在 `reference/snippets/cloudcompare-trihedron/`。

## 构建 / 环境（硬约束）
- 测试/基准必须 `TMPDIR=/home/noke/Code/trove/target/tmp`（/tmp 仅 10 MB tmpfs，否则几十个测试假失败）。
- trove-app 链接吃内存：`RUSTFLAGS="-C link-arg=-Wl,--threads=1"`（`--` 后的参数归 libtest，要放 RUSTFLAGS）。
- 沙箱内 `git push` 不可用（出口代理 502）→ 推送交用户在真实终端执行。
- clippy 基线生产代码 **0 告警**；测试基线 trove-core 322 + trove-app 20（跳过 2 个真机 GPU）。
- 依赖硬约束：`resvg` ^0.46、`tantivy` = 0.26（升 `INDEX_VERSION` 才可升）、`image` 特性只在 workspace 声明一处、`sha2` 留 0.10（oo7/ashpd 要求）、gpui-kit 跟 main 分支。
- Conventional Commits（英文），按功能拆粒度；`.workbuddy/memory/*.md` 被 git 追踪。
- 版本号在 `crates/trove-app/Cargo.toml`（现 0.4.2），UI 用 `env!("CARGO_PKG_VERSION")`。

## 环境限制（结论可信度）
- 执行器与宿主隔离（独立 PID namespace、无 systemctl/D-Bus/GUI）→ GUI 侧测量（滚动等）只能用户自己采。
- swap 曾被吃到 94.6%、目标盘 btrfs 已用 81% 且碎片化 → 系统负载漂移会让跨坐次数字失真。
- ⚠️ 基准方法论：**同坐次内 A/B**；计时边界把准备工作移出 `Instant::now()`；批量事务 vs autocommit 差 100×，不公平对比能造出假结论。

## 线程模型 / 异步
- `Library`（rusqlite `Connection`）不是 Send：DB 全在主线程，后台只做纯 IO/计算回主线程提交。
- `Context::spawn` 闭包 `(WeakEntity<T>, &mut AsyncApp)`；`background_executor().spawn` 会 move 变量，外层还要用就先 clone。
- 拿 `Window` 的唯一正解：`cx.spawn_in(window, async move |panel, cx| ...)`。`panel.update(cx,..)` 无 `Window`；`Entity::update_in` 的 `C: VisualContext` 不满足 `AsyncApp`（E0277）。

## 渲染 / 滚动（已定案）
- ✅ 全屏卡顿 **不是性能问题**，是 debounce 状态机两缺陷（修复 `f35da88`）：① 150ms 定时器只 `notify()`，**不保证排帧** → 必须 `App::refresh_windows()`（`Window::refresh`/`notify` 都可能静默 no-op）；② 一步到位的宽度跳变不该走拖动防抖 → `WIDTH_JUMP_PX = 120.0` 分流。探针实测 `render` 单帧 0.5ms，cell 构造 0.33%/s，**「每帧重建 cell」不是瓶颈**。
- gpui list 内核：判定好、重建差 —— 可视行每帧无条件跑 build 闭包（`list.rs:1074`），无元素缓存；宽度变化全量作废行高缓存（trove 靠 150ms debounce 缓解）。想跨帧复用只能自己包 `Entity`。
- 🔴 gpui `loading_assets` 是无上限 FxHashMap（`app.rs:548`），只有显式 `remove_asset` 才淘汰 → 网格缩略图从不淘汰 = 真泄漏。
- 🟢 每帧热点已修（`perf(workspace)`，2026-09-14）：`title_label`/`visible_assets`/每帧快照 clone/`page_guard` 全部按 `controller.generation` 缓存或惰性化。快照类字段一律 `Rc<Vec<_>>` + `Rc::make_mut`（`.clear()` 会 E0596；`action_targets()` 契约返回 `Vec<Uuid>`）。
- ⚠️ 探针方法论（可复用）：`thread_local` 计数器 + Drop guard，**flush 在 render 结束时**；标注 `TEMP … PROBE` 便于 grep 清场，用完即删。`cell_ms/s` 可能 > `render_ms/s`（item builder 多在 render 没跑的帧跑），逐项相加会低估。
- 滚动性能**至今未实测**；要量化只能用外部 profiler 或对比 digiKam、CloudCompare。

## 导入管线（已收敛）
- 四层：`media/import.rs`（stage_all 并行）→ `tasks/import.rs::run`（COMMIT_BATCH=16 分块事务）→ `jobs.rs`（80ms 轮询）。旁路：`import_files`、`services/collect.rs`。
- 🟢 `metadata::mine` 的 `dominant_colors` 改吃缩略图，大图 146 → 3.27 ms/文件（不要再二次解码）。
- 🟢 `add_asset` 无成本（~0.14 ms/文件）；「7.8ms/文件」是 autocommit fsync 税的误判。
- 🟡 staging 并行加速比强相关于文件大小（小图 1.31× → 大图 4.21×，4 线程）；小文件瓶颈在 FS 元数据锁。
- 🔴 `commit_staged` dedup 分支 `set_rel_path` 缺空串守卫（import.rs:330-332）：linked 导入 `rel_path=''` → 资产指向库根目录。
- 🔴 重复导入无短路：第二次 stage 仍花 0.45–0.52× 时间（mine + VisualSignature 照跑，结果被 dedup 丢弃）。
- `expand_dirs` 在 UI 线程同步跑（jobs.rs:178）；视频缩略图走 ffmpeg 同步子进程（最贵单步）。

## 查询与面板（不变量）
- `AssetQuery` 无 `text` 字段（故意删）。自由文本 → `TextIndex::search` 出 id 名次 → `assets::rank_intersect` 求交，rank 路径必须候选 id 驱动（`WhereMode::{Driving, Rejecting}`，否则 planner 选 `idx_assets_trashed` 全扫）。测试 id 数要上百。
- `id_list` 两个静默 bug 已修：占位符从 `args.len()+1` 起、空列表渲染 `IN (NULL)`；`build_where` 有 `debug_assert_eq!` 兜底。
- render 里的库读必须按 `controller.generation` 缓存（模板 `ExplorerPanel::snapshot_cache`）；**`active_*` 选中状态不要缓存**；缓存写法别让 `match &self.f` 绑定活过 match（E0502）。
- `distinct_exts` 别 `LOWER(ext)` 包索引列（改 `SELECT DISTINCT ext` + Rust 侧 folding）；候选迁移 partial index `ON assets(ext) WHERE trashed_at IS NULL`。
- 搜索：改 schema/查询语义必须 +1 `search.rs::INDEX_VERSION`。`drain` 铁律：先 `index.commit()` 再删队列行，整批一条 DELETE 包 `unchecked_transaction`，`DRAIN_BATCH=8000` 别调小。`asset_fts` 已删除并被测试钉住。
- ⚠️ 别在单一基数数据集上量索引（压测库全是 ext='jpg'）；别在单测断言索引计划。

## 三维模型 / PLY（硬约束）
- CPU/GPU 共用 `render3d::{Framing, VertexData, PointData}`；点云由 `Mesh::is_point_cloud()` 分流；逐点颜色唯一决定点 `render3d::base_color`。
- `PointData` 步长 9 f32：改它要同步改 `STRIDE`、`gpu3d.rs` `vertex_attr_array!`、`gpu3d.wgsl::vs_point`（有校验测试）。uniform 9 成员、UNIFORM_SIZE=192。wgpu 29 对齐 gpui 内置版（`immediate_size: 0`、`bind_group_layouts: &[Option<_>]`）。
- WGSL 无 GPU：naga dev-dep，parse + validate + Layouter 断言 offset；改 shader 后跑 `cargo test -p trove-app`。
- 索引只读不建：侧车 `<file>.trovecloud` 在才走 `IndexedCloud`，否则流式。转视锥只能 `Framing::frustum()`（列主序 + 深度 0..1，平面提取要转置；近平面 z=0）。批量点云用 `insert_points`。
- 流式加载别每块全量空间查询：每文件只精化 `STREAM_REFINEMENTS=32` 次（否则 40M 点 3.8s→69.5s）。
- GPU 后处理 EDL+补洞在 `gpu3d.wgsl::fs_edl`，开关 `AppConfig.point_enhance`（默认开，每帧重读）。
- meshlet：大网格（法线 + ≥8192 面）Morton 聚簇 4096 面逐簇剔除。`load_mesh` 仅 `.ply`>512MiB 走分块器（>2GiB 报错），别把 OBJ/STL 送进去。
- `max_points` 是采样预算不是「最近 N 点」：LOD 与选块都要铺满可见区。

## i18n / 主题 / gpui 坑
- locales `{en,zh-CN}.toml` 同键同序，插值 `%{var}`；快捷键显示名两处来源（`shortcuts.actions.*` + `settings.rs::action_label`）。
- 主题入口唯一 `app/theme.rs::apply_from_settings`，appearance 观察者必须传 `Some(window)`（Linux RefCell 借用 panic）。内置 36 主题由 build.rs 从 gpui-kit 生成；用户主题 `<config>/trove/themes/*.json`。
- sprite atlas 永不淘汰：换帧必须 `window.drop_image(old)`；自建帧退出路径也要归还（`viewport3d.rs::release`）。
- `SliderState::set_value(value, &Window, cx)` 三参，程序化同步只能在 render 里做。
- `context_menu(...)` 只转发 child：`on_drag`/`on_drop` 必须挂它之前；`on_prepaint` 挂 `.id()` 之前。
- `trove_core::media::mesh::Bounds` 遮蔽 gpui 的，同文件 `use …::Bounds as MeshBounds`。
- 面板模块测试别 `use super::*`（gpui prelude 带进 test 宏 → recursion limit），显式列名。
- `panel!` 宏第三段可选字段已扩；宏参数里用 `//` 别用 `///`。只有 FoldersPanel 走宏。

## 历史 / 已修正
- Open With 删的是 `.desktop` 扫描 + 复制到 `<config>/edit/`；`services/open_external.rs`（`plan`/`open`）仍在用。
- ffmpeg/ffprobe 是可选运行时依赖（只执行不链接），缺失降级：视频图标缩略图、静态海报预览、moov-only 探测。
- 已回滚/已删：A′ 三条（`Rc<[..]>`/`AssetsDrag`/`ElementId::Uuid`，`4b792d3`）、全部临时探针（perf_probe.rs、SCROLL-PROBE.md、TEMP RESIZE PROBE）；路线 B/C′ 自写虚拟化不需要。
