# 模型预览优化 · 路线图与交接文档

> 目标读者：重启机器之后继续这项工作的自己（或下一个接手的会话）。
> 写于 2026-09-13。**当前状态：P0/P1/P5 已完成；T1（索引读取侧）已完成；T5、T6（内存版）、T8 已完成；GPU 环境已恢复。剩下 T2/T3/T4/T7 —— 都依赖“建索引”，而用户已决定不建。**

---

## 0. 一句话状态

大模型预览的**内存问题已经解决**（流式常驻预算 + 抽稀，20 GB 不会 OOM）、**观感已经改善**（眼罩光照 + 补洞 + 闭合网格背面剔除）、
**索引已经能建、也已经能用**（离线建索引，打开时按可见块读取：实测 40 M 点首帧 69 ms、填满 4 M 常驻预算 4.2 s、峰值内存 350 MB），
**GPU 环境也已恢复**（重启后内核/用户态驱动一致，RTX 3050 能被 wgpu 拿到，`u64 atomics = yes`）。
还没做的是**显存侧的分页与可见性渲染**（T2–T4）与**索引体积**（T7）—— 这三项都只在“建索引”的前提下才有意义。GPU 后处理（T5）、流式加载性能、网格的 meshlet 剔除（T6 内存版）与 T8 收尾本轮已补完。

---

## 1. 目标与验收标准

| 目标 | 验收标准 |
|---|---|
| 打开 20 GB 点云 | 有索引时 < 1 s 出第一帧（不是 4–5 分钟全扫）；内存 < 512 MB；无索引时退化为今天的流式加载 |
| 大模型可交互 | 帧时间与点数**脱钩**（只与可见块数有关）；拖动时模型跟随鼠标，无"松手才动"、无"松手后一直模糊" |
| 20 GB 不崩 | 常驻内存有硬上限（当前 4M 点预算 ≈ 130 MB + 索引块缓存预算） |
| 磁盘不翻倍 | 超过 500 MB 的文件**原地链接**，不复制进库 |
| 质量 | 点云有眼罩光照（EDL）与补洞；闭合网格可剔除背面；相机转动时模型尺度稳定 |

---

## 2. GPU 环境（**已解除**，2026-09-13 重启后核实）

> 2.1 是重启*前*的诊断记录，保留作对照；2.2 的清单已逐条通过，实测结果见 2.2 之后。

### 2.1 诊断结论（重启前的记录）

```
/proc/driver/nvidia/version        → 610.57.04   ← 内存里跑的内核模块（旧）
modinfo nvidia                     → 615.71.09   ← 磁盘上为新内核构建好的模块（新）
nvidia-utils / nvidia-open-dkms    → 615.71.09   ← 用户态（新）
→ NVML 版本不匹配：nvidia-smi 失败；615 的 Vulkan ICD 建不出设备；vulkaninfo 一个 GPU 都找不到
```

原因是 2026-09-12 19:30 升级了驱动（dkms 在 19:31–19:32 为新旧两个内核都构建了模块，19:33 重建 initramfs），**但之后没有重启**，内存里仍是旧模块。模块被 172 个引用占着（KDE Plasma/kwin_wayland 正跑在 NVIDIA 上 + Xorg + Xwayland + plasmashell），无法热卸载。

**没有其他坑**：活动内核与已装内核一致；`/etc/mkinitcpio.conf` 是 `MODULES=()` 且 initramfs 里没有 nvidia 模块（不会从 initramfs 捞出旧模块）；模块集齐全（nvidia / drm / modeset / uvm / peermem）；**Secure Boot = 0**，模块另有 DKMS 签名。

### 2.2 重启后逐条验证

```bash
cat /proc/driver/nvidia/version        # 期望 615.71.09
nvidia-smi                             # 期望列出 GeForce RTX 3050 Laptop GPU
vulkaninfo --summary | head -20        # 期望出现 deviceType = DiscreteGpu
cd ~/Code/trove && cargo run           # 期望终端两行：
#  trove: adapter found: NVIDIA GeForce RTX 3050 Laptop GPU · DiscreteGpu · Vulkan · driver ...
#  trove: 3D backend ... — discrete GPU · 4× MSAA · Bgra8Unorm · storage … · u64 atomics yes/no
# 视口右上角：GPU · NVIDIA ...
```

**实测（2026-09-13 晚，重启后）**：`/proc/driver/nvidia/version` = 615.71.09，`nvidia-smi` 正常，`vulkaninfo --summary` 报 `PHYSICAL_DEVICE_TYPE_DISCRETE_GPU`。用与 `gpu3d.rs` **完全相同**的 wgpu 29 配置（`Backends::PRIMARY`、同一 `request_device` 描述符、同一格式/MSAA 选择）探测：枚举到 1 个适配器且 `request_device` 成功 ——
`NVIDIA GeForce RTX 3050 Laptop GPU · DiscreteGpu · Vulkan · driver 615.71.09`，`4× MSAA · Bgra8Unorm · storage 2047 MiB · u64 atomics yes`，`MULTI_DRAW_INDIRECT = true`。
所以 `u64 atomics` 的值决定 T4 走哪条分支（见 §4.4）——这里走有 64 位原子的那条。`/usr/share/vulkan/icd.d/` 仍只有 nvidia 的 ICD，§2.3 不需要做。

### 2.3 备选：不重启也让 trove 用上 GPU

本机是混合显卡：`card0 = 10de:25a2 (nvidia)`、`card1 = 8086:46a6 (i915, Iris Xe)`，但 `/usr/share/vulkan/icd.d/` 里**只有 nvidia 的 ICD**。

```bash
sudo pacman -S vulkan-intel          # i915 已在跑，装上即有可用的 Vulkan 设备
```

我们的适配器选择会**逐个尝试**（见 §3.6），坏掉的 NVIDIA 节点会被跳过，trove 直接用核显；重启修好 NVIDIA 后自动回到独显优先（排序：独显 → 核显 → 其它 → 软件，软件一律拒绝）。

### 2.4 若重启后仍不行

```bash
dmesg | grep -iE "nvrm|nvidia"                     # 新模块是否加载
cat /sys/module/nvidia_drm/parameters/modeset      # Wayland 需要 Y
```
换 `nvidia-dkms`（专有模块，与 `nvidia-open-dkms` 冲突，需先移除）或启动 `linux-lts`（615 也为它构建了）对比。

---

## 3. 已完成（含证据）

测试基线：**310 个 `trove-core` 测试 + 21 个 `trove-app` 测试通过；`cargo fmt --check` 干净；clippy 无新增告警**（仓库既有的 `library.rs`/`point_cloud.rs`/`stl.rs`/`page_table.rs` 告警保持原样，别把它们算成新债；`bvh.rs` 已删，告警随之消失）。`trove-app` 里有 2 个真机 GPU 冒烟测试（EDL、meshlet 剔除）：无适配器时自动跳过，本机（RTX 3050）会真跑。

### 3.1 交互：跟手、平移、方向（`trove-app/src/components/preview/model.rs`）

| 内容 | 说明 |
|---|---|
| 事件驱动重绘 | `on_mouse_move` 里补 `pump()`。**这是"松手才动"的根因**：以前拖动期间一帧都不重绘（`measure()` 在尺寸不变时提前返回，没人调用 `pump`） |
| 平移 | `Camera::pan`，实现为**枢轴与眼点一起平移**（`Framing::center` 变成枢轴）。所以平移后旋转仍绕"你看着的点"转。鼠标映射：LMB 旋转、MMB / Shift+LMB 平移、滚轮缩放、双击复位、`Shift+方向键` 平移 |
| 方向语义 | 拖动 = **模型跟随指针**（grab-and-turn）；方向键转模型同向，Shift+方向键推视角。两条约定各由一个测试钉住：`growing_yaw_swings_the_model_left`、`growing_pitch_slides_the_model_down` |
| 节流不再丢帧 | `frame_action(dirty, in_flight, dragging, since_last_frame) -> {Render, Defer, Idle, Retry}`，被节流时 `retry_later()` 用定时器补一次（多次请求合并成一次）。**这是"松手后一直模糊"的根因**：节流把请求直接丢掉且无人重试。测试 `the_frame_rule_holds_a_frame_back_instead_of_dropping_it` |
| 交互草稿帧 | 拖动中：0.5× 分辨率（长边不低于 240px，等比）+ 关 MSAA + CPU 路径 25% 几何；松手出一张满分辨率成品帧。`INTERACTIVE_SCALE` / `INTERACTIVE_MIN_EDGE` |
| 拖动中不切 LOD | 避免在相机移动时重传整个网格；松手后再应用 |
| 稳定取景 | `scene_bounds`：取景**不再用"当前显示网格"的 AABB**（流式点云的可见网格是"离相机最近的 30 万点"子集，其 AABB 随相机与加载变化 → 旋转时模型反复缩放）。改为：静态网格用原始网格 AABB；流式云用"文件采样范围 ∪ 已加载点范围"（都与相机无关、只增不减） |
| 焦点与按键 | canvas 加 `track_focus` + 首帧 `window.focus` + 点击聚焦 + `cx.stop_propagation()`（以前 WASD/QE/R 根本不响应） |

### 3.2 渲染质量（`trove-core/src/media/render3d.rs`）

- **`RenderOptions { cull_backfaces, enhance_points }`**：默认全关，所以 `render()`（缩略图/测试）行为不变。
- **眼罩光照 + 补洞**（`enhance_points`，照 Nimbus 的 `EDL.comp` + `ComposeImage.comp`）：
  1. 建 `-log2(depth)` 图（比较两个深度之比 → 一次减法）；
  2. 四邻域 `exp(-mean(max(0, l-l_n)) * 1.6)` 压暗背向/褶皱/轮廓；
  3. 3×3 支撑判据补洞（完全包围、或八个五像素 domino 方向都有支撑），**在超采样分辨率上做再降采样**，光照才不被平均掉。
  - 只在点云上生效、只在**成品帧**生效（草稿帧跳过以保帧率）。
- **背面剔除**：`Mesh::winding()`（有向边配对判闭合一致 + 有符号体积定朝向 → `TwoSided/ClosedOutward/ClosedInward`，>200 万面直接判 TwoSided），只有闭合外向网格才剔，开放壳保持双面。测试：闭合立方体开/关剔除**画面逐字节相同**；开放壳从背后看被剔除后**完全消失**（说明必须设门槛）。
- CPU 路径缓冲复用（`Scratch`，含 `log_depth`），百万顶点网格不再每帧分配 ~50 MB。

### 3.3 大文件安全网（`formats/point_cloud.rs` + `formats/streaming_point_cloud.rs`）

- `StreamingOctree` 增加**常驻点数预算**（`DEFAULT_RESIDENT_POINTS = 4_000_000`，≈130 MB）：超预算时整棵树**等距抽稀一半**并把接收步长翻倍 → 内存有界、**范围与形状保留**（不是只留文件开头），细节均匀变稀。`set_budget` / `kept_points` 可调可观测。
- 进度按**已读点数**统计（`StreamStep.points_read` vs `points_loaded`），否则抽稀后进度条会卡住；状态栏显示 `已显示 / 已读 / 共`。
- 打开文件时**采样真实范围**（`PointStreamer::sample_bounds`：4096 次等距随机读 + 末条记录，机械盘也可接受；ASCII 无法采样会返回 None）→ 取景从第一帧就正确，不会随加载缩放。
- **逐块批量插入**（2026-09-13 补）：`step` 原来逐点 `insert_point`，而它每 1 万点重建一次整棵树。改为一次 `insert_points`（`insert_points` 内部循环 `decimate` 直到回到预算内，所以内存上界不变）。
- **渲染网格限频**（2026-09-13 补）：`render_mesh_all` 是一次全量空间查询，却原来每个 5 万点的块都重建一次。现在每份文件只精化 `STREAM_REFINEMENTS = 32` 次（首步 + 完成时必做），中间的步只入八叉树、不动画面，状态栏仍每步更新。

**实测**（release，真实 40 M 点 / 572 MiB `bench-data/preview_cloud_huge.ply`，无索引走流式）：

```
逐点插入 + 每步重建    69.5 s
批量插入 + 每步重建     1.8 s   (800 步只算 step)
批量插入 + 限频重建     3.8 s   (33 次渲染, 首帧 9 ms, 峰值 RSS 301 MB)
```

即：不建索引也能 4 秒内把 40 M 点云完整加载并显示——这也是没有索引时用户实际走的路径。

### 3.4 索引 P1：已经可用（`trove-core/src/media/index/`）

| 文件 | 内容 |
|---|---|
| `order.rs` (544) | `Grid`（包围盒 → 2^bits 立方体网格，`quantise`/`dequantise`/`max_error`/`from_parts`）、`morton_code`（**按字节查表**实现位交错）、`sort_spatially`、`chunked`、`Chunk` |
| `sort.rs` (577) | `SpatialSorter`：**外存归并排序**。缓冲 `capacity` 点（默认 4M ≈ 128 MB）→ 稳定排序 → 落盘成 run → 结束时按 **16 路分组归并**（打开文件数有界）；只用一个 scratch 文件，`Drop` 守卫自动删（20 GB 会留下 20 GB 临时文件）；`SortedPoints` 流式输出；`spilled_runs()` 报告是否落盘 |
| `file.rs` (932) | `IndexConfig` / `IndexSummary` / `PointBatch` / `BatchSource` / `build_index` / `CloudIndex`。格式：96 B 头 + 载荷（**9 B/点带色、6 B/点不带**；位置是**块内 u16 分数**，颜色 u8）+ 40 B/块的块表。**块表写在载荷之后并回填头部**（一趟写完、不用预先知道点数、无二次拷贝）；**读块用独立句柄**（无锁、无需 `&mut`，可多线程） |
| `ply.rs` (351) | `build_ply_index`（采样范围定网格 → 一遍流式读取建索引）/ `build_ply_index_with_grid`（ASCII 兜底）；mesh PLY 明确拒绝 |
| `examples/index_build.rs` (99) | CLI 工具，打印源/索引体积、每点字节、耗时吞吐、重开耗时、单块读取耗时 |

**实测（release，真实 PLY：3 M 点 / 42.9 MiB 噪声球壳 + uchar 颜色）**

```
index    25.8 MiB  (60% of the source, 9.0 bytes/point)
         3000000 points in 46 chunks, colours true, 0 runs spilled
build    0.5 s  (5.6 Mpoints/s), scratch cleaned up
reopen   0.0 ms
one chunk 65536 points in 0.80 ms
```

**外推 20 GB**（15 B/点的 PLY ≈ 14.3 亿点）：建索引 ≈ 4–5 分钟（CPU 4.3 min + 读盘 10–40 s），**内存上限 ~128 MB**，索引 ≈ **12.9 GB（60%）**，打开瞬时，读一块 0.8 ms。索引是**可删的派生物**（能从源文件重建）。

**测试保证的性质**：文件里的顺序**就是**内存空间排序的顺序（逐点最大误差 3.8e-5）；局部性 = 相邻记录平均间距 ≈1.5 网格单元（文件原序 6.6）；落盘与内存结果逐点一致（含颜色不串）；块无缝铺满且包围盒包含自己的点；错 magic/未来版本/截断表/截断载荷/越界块全部报错不 panic；空云合法；scratch 用完即删。

### 3.5 缩略图不再全量扫描（`media/thumb.rs`）

`card_source(path, size)` 三档：≤64 MB 整文件解析；PLY >64 MB 走分块加载器；**PLY >512 MB 改为采样 32768 个点画卡片**（用于 20 GB 文件，不再通读一遍）。顺带修掉一个既有 bug：>64 MB 的**非 PLY**（OBJ/STL/GLB）以前被误送给"只认 PLY"的分块加载器，导致没有缩略图。

### 3.6 GPU 诊断与适配器选择（`preview/gpu3d.rs`）

- 启动时逐条打印枚举到的适配器（名字/类型/后端/driver），失败时打印原因（无适配器 / 跳过软件光栅器 / 设备创建失败并继续尝试下一个）。**这是回答"为什么走 CPU"的唯一入口。**
- 适配置选择：按 独显 → 核显 → 虚拟 → 其它 排序，**逐个尝试 `request_device`**，坏的那个不再拖累可用的那个。
- **不再请求 GL 后端**（`Backends::PRIMARY`）：GL 是唯一走 EGL 的后端，探测它只会制造 Mesa 的 `failed to create dri2 screen` 警告，而它唯一能给的软件适配器我们本来就拒绝。本机实测：警告消失。
- 能力探测 `DeviceCaps`：`int64_atomics`（**wgpu 29 里叫 `Features::SHADER_INT64_ATOMIC_MIN_MAX`**）、storage/buffer 上限、MSAA 采样数、颜色格式、设备类型。
- 其他既有改进：优先 `Bgra8Unorm`（省一次回读 swizzle，不支持则回退 RGBA）、离屏目标按 (宽,高,采样数) 缓存、成品帧/草稿帧两套采样管线（4×/2× 与 1×）、`estimate_gpu_bytes` 按真实布局修正（flat 网格曾少算 3 倍）、闭合网格归一化绕向后走 `cull_mode: BackFace`。
- 状态行为简化取舍：草稿帧降分辨率时 **GPU 路径暂无 EDL/补洞**（那是 T5）。

### 3.7 大文件原地链接（`media/import.rs` + config + 设置页）

`ImportPolicy { link_all, link_over }`，默认 `LINK_OVER_MB_DEFAULT = 500`：**≥500 MB 的文件无论导入方式如何都原地链接**（只记录路径、不复制），避免几十 GB 的模型白占一份磁盘。设置 ▸ 通用 ▸ "超过此大小自动链接（MB）"可调，填 0 关闭。

### 3.8 T1：索引接进打开路径（读取侧；**trove 自身不建索引**）

按用户 2026-09-13 的决定：**产品永不构建索引，只使用已经存在的索引**。索引由离线工具生成，默认输出 `<源文件>.trovecloud`（同目录侧车）。打开大点云时若侧车存在且能开，就走索引路径；否则原样回退到分块流式（无索引仍可打开）。

- **新增 `media/index/view.rs`**：`IndexedCloud` —— 索引的渐进读取器。`index_path_for(source)` 给出侧车命名；`step(frustum, camera)` 按「视锥内 ×（近端优先 ⋈ 全局铺开）交错」读块，单步 ≤400 k 点、绝不越过常驻预算（默认 4 M，与流式共用同一常量），每个块经一次 `insert_points` 批量灌进 `StreamingOctree`；`render_mesh(camera)` 做均匀抽稀 LOD。预算满或读完即 `complete`；读块出错只跳过该块，不让一个坏块停住整条路。
- **两点云 LOD 不再“只取最近”**（`point_cloud.rs::spread_sample`）：`to_mesh_lod` 原来把 `max_points` 理解为「最近 N 点」，于是大点云的整机视图只剩下相机面前的一片稠密补丁（实测 40 M 点云只画出 `[0.65,0.76,0.34]..[1.24,1.18,0.45]` 一个小块）。现在先把可见点全部收集（`collect_visible` 按最近叶优先走、所以抽稀会同时够到远端叶），再用 `stride = 可见数 / 预算` 均匀取样，整个轮廓都有点。`a_point_budget_still_covers_the_whole_cloud` 钉住。
- **选块顺序同时铺开全局**（`IndexedCloud::pending_order`）：原来纯按距离升序，预算全花在相机那一侧，整机视图同样只剩一块。现在把「最近序」和「索引号位反转序」（渐进细化：0, N/2, N/4, …）交错合并，一半预算给近端细节、一半铺满整个云。`the_resident_set_spans_the_cloud_not_just_the_near_side` 钉住。
- **`StreamingOctree::insert_points` 改为批量路径**：原来逐点走 `insert_point`，每 1 万点重建一次整棵树 —— 灌满 4 M 要 400 次全量重建（实测把整个填充从 4 s 拖到 **28 s**）。现在一次 append + 一次 rebuild。等价性由 `a_batch_insert_matches_point_by_point` 钉住（逐点插入 vs 批量插入产出同一棵树）。
- **新增 `Framing::frustum()` + `Frustum::everything()`**：把相机变成模型空间视锥，用于按可见性选块。顺带修掉 `Frustum::from_matrix` 的 near/far —— 它按 OpenGL `-1..1` 取 `w ± z`，而 `Framing::view_projection` 是 wgpu 的 `0..1`，会把近平面算到远平面上去（旧单测用的是 OpenGL 矩阵，所以从未暴露）。现在由 `the_frustum_matches_the_projection_matrix` 对着投影矩阵逐点核对。
- **app 侧**（`preview/model.rs`）：`start_load` 先试侧车；新增与流式完全对称的 `is_indexed` 渐进循环（`begin_index_step`，含后台步进与串行号防陈旧）；完成后把 30 万点成品网格交给 GPU 并**丢掉索引及其 400 万常驻点**；状态栏新增 `viewport.backend_indexed`「索引点云 · x/y 块」。
- **明确不做**（并入 T2）：相机移动后的**卸载/重选**（现在常驻集只增不减；转身到背面看到的是已加载那一面），以及任何索引自动构建。

**实测**（release，真实 40 M 点 / 572 MiB `bench-data/preview_cloud_huge.ply`，索引 343 MiB / 源的 60% / 611 块）：

```
first step    76 ms   (7 块, 459 k 点常驻)      ← 首帧
fill          4.3 s   (9 步填满 4.0 M 常驻预算，末步 690 ms)
peak RSS      315 MB  (VmHWM)
常驻块范围    [-1.19,-1.20,-0.50] .. [1.30,1.18,0.50]
云的范围      [-1.30,-1.20,-0.50] .. [1.30,1.20,0.50]   ← 整机都在
render mesh   283 k 点，范围同上（均匀抽稀，不再是中间一小块）
```

回归面：`index/view.rs` 8 个单测（最近块先读、预算封顶、读满即完成、盲视锥回退、颜色随点、完成后步进幂等）＋ `point_cloud.rs` 的批量等价测试；`trove-core` 308、`trove-app` 20 全绿。

### 3.9 T5：EDL + 补洞搬到 GPU（`preview/gpu3d.wgsl` + `gpu3d.rs`）

点云在 GPU 上以前**没有眼罩光照**——CPU 回退有、GPU 反而更平，这是 GPU 恢复可用后最刺眼的观感回退。现在补齐：

- 主 pass 之后加一个**全屏片元 pass**（`fs_edl`），直接读**多重采样深度附件**（`texture_depth_multisampled_2d`，取 sample 0）与已 resolve 的彩色图；输出写到一张新的 `edl_color` 纹理，回读改从它取。
- 深度到 `log2(w)` 的换算由主 uniform 的 `params2.z/w`（`z_scale`/`z_bias`，与 `render3d` 的投影同源）完成；强度用 `params2.y = EDL_STRENGTH`（已从 `render3d` 提成 `pub const`，两个渲染器不再各写一份 1.6）。
- 补洞（Nimbus 的 3×3 support + 8 个五像素 domino）与 EDL 同一个 pass，规则逐条对齐 CPU 版；唯一差异是补洞读的是**上色后**邻居（CPU 分两趟：先 EDL 后补洞），肉眼不可辨。
- **只在“成品帧 + 点云 + 有 MSAA”时运行**：草稿帧是单采样的，没有多重采样深度可读；网格不需要（`enhance_points` 本来只作用于点云）。所以 GPU/索引路径的成品帧观感与 CPU 对齐。
- 新增**真机冒烟测试** `the_eye_dome_pass_runs_on_a_real_device`：没有适配器就跳过（CI 条件），有则上传一个球面点云、成品帧渲染两次（开/关 EDL），断言两次**不同**且不是全黑——顺带证明主 pipeline 不会被 shader 里额外的 `@group(1)` 声明绊倒。

深度换个 R32Float 附件也行，但那要多一张附件、多一次编码；直接读 MSAA 深度附件（`textureLoad(..., 0)`）省掉这些，代价是要求适配器支持 MSAA——单采样时就不带 EDL，而不是显示一张错的。

### 3.10 T6/T8 收尾：meshlet 剔除、点云增强开关、大文件路由

- **meshlet 化 + GPU 视锥剔除（T6 的内存版）**：`formats/meshlet.rs` 把三角面按质心的 Morton 码排序、每 4096 面切一簇并算 AABB；`GpuRenderer::upload` 按该顺序重排索引缓冲，`render` 每帧用 `Framing::frustum()` 逐簇剔除，只画可见簇；全部可见时退回一次 draw（避免几千次 draw call）。只对**带顶点法线（索引布局）且 ≥8192 面**的网格生效：平面着色布局每个三角形自带 3 个顶点、没有索引可跳，切簇只会更亏。测试：`meshlet.rs` 5 个（覆盖一次、盒子包含、小网格/点云/平面着色不切、单簇）＋ 真机 `a_large_mesh_is_partitioned_and_culled_on_a_real_device`（131k 面网格：远看全可见→单 draw，近看部分剔除→`Some(ranges)`，并真画一帧）。
- **点云增强开关（T8）**：`AppConfig.point_enhance`（默认开）→ 设置 ▸ 通用 ▸“点云增强”按钮；视口每帧重读配置，所以切开关能作用到已打开的预览；同时驱动 CPU 的 `enhance_points` 与 GPU 的 `fs_edl`。
- **大文件路由 bug（T6 顺带）**：`load_mesh` 原来把**任何** >512 MiB 的文件都交给“只认 PLY”的分块加载器，于是大 OBJ/STL 会报一个“不是 PLY”的错。现在只有 `.ply` 走分块，其余交给整文件加载器（它有自己的 2 GiB 上限和清楚的消息）。
- 删除死模块 `formats/bvh.rs`（`#[cfg(test)]`、无人使用）。

---

## 4. 未完成（按建议顺序）

> 本节的每一项都另有**可操作的存档**（目标/设计要点/改动位置/依赖/验收/阻塞原因）在
> [`MODEL-PREVIEW-BACKLOG.md`](./MODEL-PREVIEW-BACKLOG.md)；这份路线图只留状态与判断。

### 4.1 T1 把索引接进打开路径 —— **读取侧已完成（见 §3.8）**

实现与实测都在 §3.8。留下的部分，按设计并入 T2：

- **相机移动后的卸载/重选**：现在常驻集合只增不减（最近块优先填到预算满就停）。转身到背面时看到的是已经加载的那一面；把重选放到 T2 的 GPU 分页里做才划算（每帧上传/驱逐本来就在那边）。
- **构建索引**：明确不做（见 §6 决策 2）——只能离线 `index_build` 生成，trove 只读。

原验收（首帧 <1 s、峰值 <512 MB、状态栏 "索引 x/y 块"、删掉索引仍能打开）均已满足；"有索引时 <1 s" 改为实测 69 ms。

### 4.2 T2 VRAM 池 + 分页（P2，需要 GPU 验证）

- **目标**：显存只放"当前可见"的块，超出预算 LRU 驱逐。
- **设计要点（比 Nimbus 简化）**：**固定大小 slab 分配器**（每 LOD 层一组槽位）+ 上传用 staging ring，**因此不需要** Nimbus 的 `CompactPositionsBuffer`/`CopyPositions` 那两个搬移 pass 与配套 fence；段级 fence 用 `on_submitted_work_done` 或按帧提交序号判定。
- **改动位置**：新增 `preview/cloud/{pool,cache}.rs`；`gpu3d.rs` 增加 storage buffer 绑定与上传路径。
- **验收**：8 GB 显存机器上 20 GB 文件帧时间恒定；上传字节 ∝ 新进入视野的块；状态栏显示常驻字节/命中率。

### 4.3 T3 GPU 节点剔除 + 间接绘制（P3，需要 GPU）

- **内容**：`MeshletCulling.comp` 等价 WGSL —— 每块 AABB 与 6 个视锥平面（由 `Framing::view_projection` 在 CPU 侧算出后上传）测试，写每块可画点数/间接绘制列表；`multi_draw_indirect`（先查 `Features::MULTI_DRAW_INDIRECT`，不支持则逐块 draw）。
- **可验证部分（不需要 GPU）**：把平面提取与 AABB 判定写成 Rust 函数并单测（作为 GPU 实现的"标准答案"），WGSL 用 `naga` 解析+校验（沿用 `gpu3d.rs` 里既有的 shader 测试套件）。
- **验收**：CPU 每帧工作量 ∝ 可见块数；放大到局部时上传/绘制量显著下降。

### 4.4 T4 点云 compute 光栅化 + 可见性缓冲（P4，核心，需要 GPU）

- **内容**：点云绘制从"顶点+片元 sprite"换成 compute 写可见性缓冲（`ResetDepth → Raster`），随后 T5 的 compose pass 出图。
- **能力分支**（`DeviceCaps.int64_atomics`）：
  - 有 `SHADER_INT64_ATOMIC_MIN_MAX` → 照 Nimbus：`atomicMin(u64)` 打包 `depth<<32 | 属性`（一次原子同时得深度与"这个像素是哪个点"）；**注意：目前设备创建时 `required_features: Features::empty()`，走这条分支必须按能力请求该 feature**。
  - 没有 → `atomicMin(u32 depth)` + 一遍"胜者回写颜色"的 resolve（约 +15~30% 光栅开销）。
- **保留**现有 sprite 路径作为小文件快路径（**需你确认**，见 §6 决策 4）。
- **验收**：观感不低于现有 splat；大点云帧时间下降；无 GPU 时仍走 CPU。

### 4.5 T5 后处理搬到 GPU（EDL / 补洞 / 遮挡清理）—— **已完成（见 §3.9）**

EDL + 补洞已在 GPU 上实现（全屏片元 pass 读多重采样深度附件）。剩下的只有 **Nimbus 的"遮挡清理"**（3×3 夹角阈值剔除被遮住的点）：对干净扫描会删细节，没做、也不建议默认开。

"草稿帧也带 EDL"（原 T8）**有意不做**：草稿帧是单采样的（无多重采样深度可读），而且它要被放大回画布，算出来的折痕也活不下来。与 CPU 路径的 `enhance_points: !interactive` 对齐。

### 4.6 T6 网格路线 —— **内存版已完成（见 §3.10）**

已完成：meshlet 化（Morton 聚类 + AABB）+ GPU 逐簇视锥剔除（对应 Nimbus 的 `MeshletsAABB`）。
**未做**：分页（对应 `TriangleMesh` 的持久化侧车）—— 它和点云索引是同一笔磁盘账（网格还要存顶点/索引，只会更大），用户已明确不建侧车，所以不做。
今天的现实没变：PLY 网格 >512 MB 走分块加载器（内存有界但通读一遍 + 按 stride 抽稀，细节永久丢失），不加侧车就无法改成按需取块。

### 4.7 T7 索引体积（可选优化）

当前 9 B/点（位置 3×u16 + 颜色 3×u8）。可选：① 去掉颜色存索引 → 6 B/点（20 GB → 8.6 GB）；② 位置压到 **11-10-11 位 = 4 B/点**（Potree 做法，20 GB → 10 GB，颜色保留）；③ 颜色降到 3-3-2 位。**需你确认取舍**（见 §6 决策 3）。

### 4.8 T8 杂项

- ✅ **增强的 UI 开关**（2026-09-13）：设置 ▸ 通用 ▸ “点云增强”（`AppConfig.point_enhance`，默认开），控制 EDL + 补洞；视口每帧重读配置，所以开关能作用到已打开的预览。
- ✅ **`formats/bvh.rs` 已删**：`#[cfg(test)]`、无人使用，9 条 dead_code 告警随之消失（测试总数 -3）。
- ~~`preview/viewport_wgpu.rs`~~ **已删**（2026-09-13）：没在 `preview/mod.rs` 里声明、从未编译，内容与现状矛盾。
- ❌ **草稿帧也上 EDL/补洞**：有意不做。草稿帧是单采样的（没有多重采样深度可读），它要被放大回画布，算出的折痕也活不下来；CPU 路径同样在草稿帧跳过。要做就得再养一套 `texture_depth_2d` 的管线变体，比收益贵。
- ❌ **遮挡清理**（Nimbus 的 3×3 夹角剔除）：对干净扫描会删真实细节，默认不该开，没做。
- ⏭️ **索引缓存复用缩略图**：没有索引，无从复用。
- ⏸️ **导入哈希政策（决策 6）**：用户选了“延迟哈希”，但它不是“晚点算”那么简单 —— sha256 现在是 blob 命名、缩略图命名、去重跳过、删除回收**四个键**。延迟就得同时改这四处（缩略图改按 asset id、删除改用别的判据、再加一个后台补哈希+合并任务），属于**存储身份模型的重构**，没动。
- `wgpu_atlas.rs`/`wgpu_context.rs` 属于 fork，不在我们控制范围。

---

## 5. Nimbus 参考架构速查（省得重读源码）

来源：`Krixtalx/Nimbus`（IEEE TVCG 2025《Virtualized Point Cloud Rendering》，**Windows/OpenGL 4.6，许可 CC BY-NC-SA → 只能借鉴思路，不能抄代码**）。

**一次性预处理**：点按 **Hilbert** 排序 → 切成 **meshlet（点簇，带 AABB）** + 多档 **band（LOD）** → 写成侧车文件 `<源文件>.NimbusCloudPos` / `.NimbusCloudRGB`（`IO/FileManager.cpp:330/544`）。论文指出 pointlet 用 Hilbert 而非 Morton 是为了减少空间跳变。

**每帧（全 compute，无顶点/片元管线画点）**：
1. `ResetDepthBuffer.comp` 清可见性缓冲；
2. `MeshletCulling.comp`：AABB 六平面视锥剔除 + **按屏幕投影尺寸连续决定画多少点**（`numPoints = clamp(numPixels * meanExtent²/extent², 0, meshletSize)`），并 `atomicAdd` 统计总点数；
3. 主机**滞后一帧**读回剔除结果 → 生成加载/驱逐任务（8 个 worker 线程 + 并发队列；RAM/VRAM 上限 0.85）；
4. `CompactPositionsBuffer` + `CopyPositions`：GPU 池搬移腾空间；
5. `ComputeDepthBuffer.comp`：**compute 光栅化点**，`atomicMin` 64 位打包 `depth<<32|属性`（依赖 `GL_NV_shader_atomic_int64`，并用 subgroup 归约做同像素早合并）；
6. `ComposeImage.comp`：3×3 邻域**遮挡夹角**剔除被遮住的点；
7. `EDL.comp`：**眼罩光照 + 3×3 补洞 + 颜色解码**，直接 `imageStore` 到 rgba8。

**我们可以改进的地方**：GPU 池用固定 slot 分配器（省掉第 4 步两个 pass）；LOD 用我们的**屏幕空间误差**（QEM 逐层误差）而不是他们的 `numPixels×mult` 启发式；呈现路径保持"离屏 + 回读给 gpui"（他们直写窗口，我们没有那个通道——要 patch fork 才行）。

---

## 6. 需要你拍板的决策

| # | 决策 | 我的建议 |
|---|---|---|
| 1 | 索引放哪：库缓存目录（按内容哈希）还是源文件旁边（`.trovecloud` 侧车） | **已定：侧车**。用户要求不建索引，因此没有内容哈希或导入记录可用于命名；侧车是唯一只凭路径就能发现的方案（大文件默认原地链接，路径就是源文件） |
| 2 | 索引何时建：导入后自动 / 首次预览时后台 | **已定：都不建**（用户 2026-09-13）。trove 只读已存在的侧车，离线用 `index_build` 生成 |
| 3 | 索引是否保留颜色 | **保留**（点云没颜色就像灰尘）；若磁盘紧张再选 T7 |
| 4 | 小文件是否保留现有 sprite 路径 | **保留**：回归面小，GPU compute 路径只对大文件启用 |
| 5 | 20 GB **网格**是否在范围内（T6 meshlet） | 先确认你的大文件是不是点云；是点云就先不做 T6 |
| 6 | 20 GB 文件的导入哈希政策 | 需要你定：全量哈希（可靠但慢）／弱哈希（快但去重可能误判）／延迟哈希 |

---

## 7. 命令速查

```bash
# 构建与检查
cargo build -p trove-app
cargo test --offline -p trove-core -p trove-app        # 期望 296 + 19
cargo clippy --all-targets -p trove-core -p trove-app  # 只看新告警
cargo fmt --all -- --check

# 索引工具（对真实文件测；release 比 debug 快约 10 倍）
cargo run --release --example index_build -- cloud.ply [out.trovecloud]

# 跑应用（打开 3D 模型时看终端两行后端诊断）
cargo run

# GPU 环境验证（重启后）
nvidia-smi && vulkaninfo --summary | head -20
ls -l /dev/dri/ ; ls /usr/share/vulkan/icd.d/
```

**代码地图**

| 关注点 | 文件 |
|---|---|
| 视口/交互/调度/取景 | `crates/trove-app/src/components/preview/model.rs` |
| GPU 管线/能力探测/适配器 | `crates/trove-app/src/components/preview/gpu3d.rs` + `gpu3d.wgsl` |
| CPU 光栅器/EDL/补洞/背面剔除/相机 | `crates/trove-core/src/media/render3d.rs` |
| 点云八叉树与预算抽稀 | `crates/trove-core/src/media/formats/point_cloud.rs` |
| 流式点云（预算、取景范围、步进） | `crates/trove-core/src/media/formats/streaming_point_cloud.rs` |
| PLY 随机访问读取器（采样/分块） | `crates/trove-core/src/media/formats/streaming/point_streamer.rs` |
| **索引**（排序/格式/PLY 接入） | `crates/trove-core/src/media/index/{order,sort,file,ply}.rs` |
| 导入策略（>500 MB 原地链接） | `crates/trove-core/src/media/import.rs` + `src/config.rs` + `dialogs/settings/general.rs` |
| 缩略图（大文件采样） | `crates/trove-core/src/media/thumb.rs` |

---

## 8. 已知的坑（别踩回去）

1. **gpui 图片元素会按"图片自身像素尺寸"布局**：`style.size` 为 `Auto` 时，框架把元素尺寸设成图片的像素尺寸（`gpui-pre-x/src/elements/img.rs` 的 `request_layout`）。所以**任何渲染帧都必须显式 `.size_full()`**，否则尺寸不同的帧会被画在左上角且变小——这正是"旋转后预览缩到左上角"的原因（半分辨率草稿帧暴露了它）。`video.rs`/`image.rs` 用的 `max_w_full/max_h_full` 是"上限、不放大"的有意策略，不要顺手改成 `size_full`。
2. **节流必须"延后"而不是"丢弃"**：`pump` 里被节流掉且无人重试的请求 = 屏幕上的画面永久停在草稿帧。
3. **`Backends::GL` 在 Linux 上会走 EGL** → Mesa 的 `failed to create dri2 screen` 警告；我们的渲染器拒绝软件适配器，所以不需要它。
4. **wgpu 29 的 64 位原子 feature 叫 `Features::SHADER_INT64_ATOMIC_MIN_MAX`**；而且设备创建时要**按需请求**（现在是 `Features::empty()`）。
5. **Morton ≠ Hilbert**：Morton 在子立方体边界会跳步（单步最大跳变远大于单元尺寸），所以只能断言**均值**局部性，不能断言"相邻必相邻"。要换 Hilbert 只需替换 `order::spatial_key`。
6. **位交错的 magic 掩码极易写错并静默丢高位**：`order.rs` 现在用**按字节查表**，并与逐位朴素实现逐值比对（测试 `morton_matches_the_naive_interleave`）。
7. **`sample_bounds`/`sample_points` 需要定长记录**：只能用于 binary PLY（ASCII 无法随机寻址）。
8. **app 的测试模块不要 `use super::*`**：gpui 的 prelude 里有一个 `test` 属性宏，glob 导入会让 `#[test]` 递归展开报错——要显式列导入（`panels/workspace/mod.rs` 已有先例与注释）。
9. **`cargo fmt` 会重排长 `assert!`**，锚点式批量编辑容易失配；改完先跑 fmt 再改。
10. **别在 UI 线程上做重活**：LOD 切换会重传网格（拖动中已跳过）、绕向判定会遍历三角形（只在几何互换时算一次，LOD 层继承）、流式步进已移到后台线程。
11. **`Framing::view_projection` 是列主序、深度 `0..1`**：`Frustum::from_matrix` 按**行**读，构造视锥前必须转置；而且 `0..1` 深度下近平面是 `z = 0`（row2）、远平面是 `w - z`，不是 OpenGL 的 `w ± z`。任一处搞错都会得到一个"看起来像视锥"却裁错半个模型/整段深度的盒子。唯一入口是 `Framing::frustum()`，由 `the_frustum_matches_the_projection_matrix` 对着投影矩阵逐点核对。
12. **别用逐点 `insert_point` 灌大批量**：它每 1 万点重建一次整棵树，灌一个 6.5 万点的块等于把已常驻的点重新索引 6 遍。批量走 `insert_points`（一次 append + 一次 rebuild），语义等价由 `a_batch_insert_matches_point_by_point` 钉住。
13. **`max_points` 是采样预算，不是“最近 N 点”**：点云 LOD 与索引选块都必须把预算铺在可见区域上（`spread_sample` / `pending_order` 的交错）。改成“按距离取前 N”后大模型预览会静默退化成相机前面一小块——不报错、也不崩，只是模型少了一大半。

---

## 9. 建议的下一步

1. GPU 已可用（§2），`u64 atomics = yes` → T4 走 Nimbus 的 64 位打包分支，设备创建时记得**按能力请求** `Features::SHADER_INT64_ATOMIC_MIN_MAX`（现在仍是 `Features::empty()`）。
2. T1 读取侧已完成（§3.8）。索引构建明确留给离线工具，**不要再往 app 里加构建流程**。
3. **T5 已完成**（§3.9），且流式路径的性能问题（逐点插入、每步重渲）本轮也修了（§3.3）。
4. **T2 → T3 → T4**：VRAM 池 → GPU 节点剔除 → compute 光栅化。这三项都是**为索引侧的大点云服务**的；用户已明确不用索引，所以优先级下降。要重启它们时，入口仍然是 `IndexedCloud`：把 `step` 的「视锥内最近块」换成按帧上传/驱逐。
5. **T6 内存版已完成**（§3.10）：meshlet 聚类 + GPU 逐簇视锥剔除，不需要侧车。T7 与 T6 的分页那半都依赖侧车，不做。
6. **T8 只剩导入哈希政策**（用户选了“延迟哈希”，但它是存储身份模型的重构：sha256 同时是 blob 名、缩略图名、去重键、删除回收键；要做先拍板改哪几个键）。bvh.rs 已删，其余已完成（§3.10）。
