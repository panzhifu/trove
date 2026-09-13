# 模型预览 · 未做优化存档（Backlog）

> 写于 2026-09-13。已完成的工作、实测数字与"别踩回去"的坑在
> [`MODEL-PREVIEW-ROADMAP.md`](./MODEL-PREVIEW-ROADMAP.md)；这份文档只装**没做**的东西，
> 目的是哪天想捡起来时不用重新调研。
>
> **前提（决定了下面每一项的取舍）**：trove 的产品定位是资产管理，模型数量多、单个通常不大；
> 用户明确**不构建 `.trovecloud` 索引**（20 GB 索引成品 13 GB、构建临时盘 130 GB+）。
> 因此所有"围绕索引的分页/剔除"都处于**搁置**状态，不是技术上做不到。

---

## 0. 现在的基线（重启工作时先确认这些还成立）

| 能力 | 现状 |
|---|---|
| 20 GB PLY 点云 | 流式加载，首帧几 ms、完整约 1 分钟，内存有界（峰值 ~300 MB） |
| 40 M 点 / 572 MiB 点云 | 流式 3.8 s（33 次渲染），首帧 9 ms |
| GPU | 可用（RTX 3050，`u64 atomics = yes`、`MULTI_DRAW_INDIRECT_COUNT = true`、4× MSAA、Bgra8Unorm） |
| 点云观感 | GPU 与 CPU 都带 EDL + 补洞；开关在 设置 ▸ 通用 ▸ 「点云增强」 |
| 大网格 | ≥8192 面且有法线的网格做 meshlet 聚类 + 逐簇视锥剔除 |
| 索引 | 只读不建；有侧车时 `IndexedCloud` 分块加载，否则流式 |
| 测试/检查 | `trove-core` 310 + `trove-app` 21（含 2 个真机 GPU 测试）；clippy 全工作区 0 告警；fmt 干净 |

代码地图见路线图 §7。测试必须 `TMPDIR=<repo>/target/tmp`（`/tmp` 只有 10 MB）。
真机 GPU 测试在没有适配器时会自动跳过，所以 CI 不会因为没显卡而红。

---

## 1. 待办项

### P1 · 提高 GPU 侧的绘制预算（**最小、不需要索引，建议第一个做**）

- **现状**：无论内存里有多少点，画面最多画 `DEFAULT_POINT_BUDGET = 300_000` 点。
  而显存上传预算是 256 MB（`PointData::STRIDE = 36 B` → 约 740 万点），流式常驻上限是 400 万点。
  也就是说**不建索引也有 10 倍以上的余量**没被用上，大点云看起来比实际稀疏。
- **怎么做**：
  1. `trove-core`：把预算变成可设置的参数，而不是常量。`streaming_point_cloud.rs::render_mesh` /
     `render_mesh_all` 与 `index/view.rs::render_mesh` 现在硬编码 `DEFAULT_POINT_BUDGET`；
     加 `set_point_budget(points)`（或把预算作参数传进去），保留 300 k 作为默认。
  2. `trove-app`：`ModelViewport` 按后端选预算 —— GPU（`gpu.is_some()`）用 200 万～400 万，
     CPU 保持 30 万（软件光栅器对点数敏感）。注意 `start_gpu` 是异步的，加载期间还不知道
     后端，所以要么加载完成后再按后端重建一次网格，要么先按 CPU 预算、GPU 就绪后重渲一帧。
- **验收**：40 M 点云在 GPU 上画出的点数从 30 万升到数百万；帧时间仍在可交互范围。
- **坑**：`GpuMesh` 的 `estimate_gpu_bytes` 会检查 256 MB 上限；点数上去了要么不超，
  要么按 `GPU_UPLOAD_BUDGET` 自动回落到 CPU。别让 CPU 路径吃到几百万点。

### P2 · T2 VRAM 池 + 分页（**需要索引**）

- **目标**：显存只放"当前可见"的块，超预算按 LRU 驱逐；8 GB 显存机器上 20 GB 文件帧时间恒定。
- **设计要点（比 Nimbus 简化）**：固定大小 slab 分配器（每 LOD 层一组槽位）+ 上传用 staging ring，
  **因此不需要** Nimbus 的 `CompactPositionsBuffer` / `CopyPositions` 两个搬移 pass 与配套 fence；
  段级 fence 用 `on_submitted_work_done` 或按帧提交序号判定。
- **改动位置**：新增 `preview/cloud/{pool,cache}.rs`；`gpu3d.rs` 增加 storage buffer 绑定与上传路径。
- **入口**：`IndexedCloud`（`media/index/view.rs`）。现在的 `step()` 是 CPU 侧"视锥内最近块"，
  要换成"按帧上传/驱逐"。
- **验收**：帧时间不随文件增大而变；上传字节 ∝ 新进入视野的块；状态栏显示常驻字节/命中率。
- **阻塞**：先要有索引侧车（用户已决定不建）。

### P3 · T3 GPU 节点剔除 + 间接绘制（**需要 T2**）

- **目标**：每块 AABB 与 6 个视锥平面在 GPU 上测试，写出每块可画点数/间接绘制列表；
  `multi_draw_indirect`（本机 `MULTI_DRAW_INDIRECT_COUNT = true`；不支持则逐块 draw）。
- **已经做完的部分**：CPU 侧的平面提取与 AABB 判定 = `Framing::frustum()` + `Frustum::intersects_bounds`，
  由 `the_frustum_matches_the_projection_matrix` 钉住。GPU 实现可以直接拿它当"标准答案"比对。
- **剩下**：`gpu3d.wgsl` 里加 compute 入口（视锥平面 + 块表 storage buffer），`gpu3d.rs` 加 pass 与 indirect buffer。
- **参考**：Nimbus 的 `MeshletCulling.comp`，以及它的
  `numPoints = clamp(numPixels * meanExtent²/extent², 0, meshletSize)`。

### P4 · T4 点云 compute 光栅化 + 可见性缓冲（**不强制索引**）

- **目标**：点云绘制从"顶点+片元 sprite"换成 compute 写可见性缓冲（`ResetDepth → Raster`），
  随后由 compose pass 出图。
- **能力分支**（`DeviceCaps.int64_atomics`）：
  - 本机有 `SHADER_INT64_ATOMIC_MIN_MAX` → 照 Nimbus：`atomicMin(u64)` 打包 `depth<<32 | 属性`。
    ⚠️ **设备创建时仍是 `required_features: Features::empty()`，走这条分支必须按能力请求该 feature。**
  - 没有 → `atomicMin(u32 depth)` + 一遍"胜者回写颜色"的 resolve（约 +15~30% 光栅开销）。
- **价值判断**：不依赖索引，但只有在"一次画几百万点"（配合 P1）时才明显划算；
  现在画 30 万点，sprite 路径够用。
- **验收**：观感不低于现有 splat；大点云帧时间下降；无 GPU 时仍走 CPU。

### P5 · T5 剩余：遮挡清理（**可选开关，默认关**）

- **已完成**：EDL + 补洞（`gpu3d.wgsl::fs_edl` + CPU 的 `render3d::enhance_points`）。
- **剩下**：Nimbus 的"遮挡清理"——3×3 邻域夹角阈值剔除被遮住的点。对干净扫描会删真实细节，
  所以**只能作为默认关的开关**，别默认开。
- **怎么做**：在现成的 `fs_edl` 里加一段（已经有 3×3 深度可读），uniform 加一个 flag，
  走 `AppConfig` + 设置页按钮（照抄「点云增强」那套即可，见 §3.10）。

### P6 · T6 剩余：网格分页 / meshlet 侧车（**需要侧车**）

- **已完成**：内存版 —— `formats/meshlet.rs`（质心 Morton 聚类 + AABB + 索引重排）+
  `gpu3d.rs` 逐簇视锥剔除。
- **剩下**：持久化的网格侧车（meshlet + 多档 LOD band），让 20 GB 网格不必整份读、
  按可见性取块。磁盘成本与点云索引同一笔账（还更大：要存顶点/索引）。
- **今天的现实**：PLY 网格 >512 MB 走 `chunked::load_ply_chunked`（mmap + 每 N 个顶点/面抽稀），
  内存有界但**通读一遍 + 细节永久丢失**、且没有渐进首帧。不加侧车就没法改成按需取块。

### P7 · T7 索引体积压缩（**需要索引**）

- **现状**：9 B/点（位置 3×u16 = 6 B + 颜色 3×u8 = 3 B），索引 ≈ 源文件的 60%。
- **候选**：
  1. 不存颜色 → 6 B/点（20 GB 源 → ~8.6 GB）；
  2. 位置压到 11-10-11 位（Potree 做法）→ 带色 ~7 B/点（20 GB → ~10 GB）；
  3. 颜色降到 3-3-2 位。
- **怎么做**：`media/index/file.rs` 的 `RECORD_COLOURS`/`RECORD_PLAIN` 与编解码，
  以及 `Header.record_bytes` 已经带版本（`VERSION`）——加新布局要**升版本**并保留旧版读取；
  `order.rs` 的网格量化精度要跟着动。测试：往返精度 + 旧文件仍可读。

### P8 · T8 剩余：导入"延迟哈希"（**需要拍板，属存储身份模型重构**）

- **用户选择**：延迟哈希。但它不是"晚点算"那么简单 —— sha256 同时是**四个键**：
  1. blob 存储名（`media/<sha前缀>/<sha>.<ext>`，copy 模式**必须先有 hash 才能落盘**）；
  2. 缩略图名（`thumb::abs_path(root, sha)`）；
  3. 去重跳过（`assets::find_by_sha256`）；
  4. 删除回收（`library.rs::purge_asset` 靠 `count_by_sha256 == 0` 才删 blob/缩略图）。
- **关键结论**：copy 模式**必须先读完整份**才能写 blob（hash 就是文件名），延迟对它没有意义；
  真正受益的只有**链接导入**（默认 ≥500 MiB 就链接）——那行 `blob::hash_file(src)` 纯粹为了算键。
- **因此可行方案**：只对**链接导入**延迟。需要一起决定：
  - 缩略图改按 asset id 命名（或延迟到补哈希后再生成）；
  - 删除时在 hash 为空的情况下改用保守判据（同 `rel_path`/大小）而不是引用计数；
  - 一个后台补哈希任务（何时跑？导入批次结束 / 空闲 / 手动），完成后写回 `assets.sha256`
    并合并因此暴露出的重复记录；
  - 不做全量哈希的代价：重复记录会短暂存在，之后清理。
- **改动位置**：`media/import.rs`、`media/blob.rs`、`media/thumb.rs`、`store/assets.rs`、`library.rs`。

---

## 2. 明确不做（连同理由，免得以后被当成遗漏）

- **草稿帧也上 EDL/补洞**：草稿帧是单采样的（没有多重采样深度附件可读），而且它要被放大回画布，
  算出来的折痕活不下来；CPU 路径同样在草稿帧跳过。要做就得再养一套 `texture_depth_2d` 的
  绑定布局 + 管线变体，比收益贵。
- **遮挡清理默认开**：见 P5，会删真实细节。
- **索引构建进 app**：产品永远只读索引，构建只留离线 `example index_build`（用户决定）。

---

## 3. 重启这项工作时最该先知道的坑

（完整清单在路线图 §8；这里只放与待办项直接相关的）

1. `Framing::view_projection` 是**列主序 + 深度 0..1**；构造视锥用 `Framing::frustum()`（内部转置），
   近平面是 `z=0`、远平面 `w−z`，不是 OpenGL 的 `w±z`。
2. `max_points` 是**采样预算**，不是"最近 N 点"；`point_cloud.rs::spread_sample` 与
   `index/view.rs::pending_order` 都是"铺开"语义，别改回按距离取前 N（大模型预览会静默退化成一小块）。
3. 批量点云走 `insert_points`（一次 rebuild + **循环 `decimate` 回预算内**）；逐点 `insert_point`
   每 1 万点重建整棵树。
4. **全量空间查询别放进每步循环**：`render_mesh_all` 是 O(常驻点数)，流式每块重建一次会把
   40 M 点加载从 3.8 s 拖到 69.5 s（现在每文件只精化 `STREAM_REFINEMENTS = 32` 次）。
5. wgpu 只校验入口点**实际用到**的资源，所以 shader 里额外的 `@group(1)` 声明不会绊倒主管线；
   但 WGSL 的深度纹理类型 `texture_depth_multisampled_2d` **不带 `<f32>`**。
6. 真机 GPU 测试是本仓库的既定做法（`GpuRenderer::new()` 失败就 return），新加 GPU 功能请照抄。

---

## 4. 命令速查

```bash
# 测试（/tmp 只有 10 MB，必须换 TMPDIR）
TMPDIR=$PWD/target/tmp cargo test --offline -p trove-core -p trove-app
# 静态检查
cargo fmt --all -- --check
TMPDIR=$PWD/target/tmp cargo clippy --offline --all-targets -p trove-core -p trove-app
# 真机 GPU 测试（没有显卡会自动跳过，本机应真跑）
TMPDIR=$PWD/target/tmp cargo test --offline -p trove-app gpu
# 打开应用看后端诊断
cargo run
# 离线建索引（只在决定重启 P2/P3/P7 时才会用到）
TMPDIR=$PWD/target/tmp cargo run --release -p trove-core --example index_build -- cloud.ply
```
