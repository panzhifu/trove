# Trove 导入管线 · 现状、实测与剩余优化

> 写于 2026-09-18 ｜ 适用版本：0.4.5（含 2026-09-18/19 的导入管线改动）
> 目标读者：重启机器之后继续这项工作的自己（或下一个接手的会话）。
> 回答三个问题：**现在长什么样**、**已经量到哪些数**、**还剩什么值得做**。
>
> 相关文档：[`MODEL-PREVIEW-BACKLOG.md`](./MODEL-PREVIEW-BACKLOG.md) 的 **P8「导入延迟哈希」**（同一件事的另一半，等拍板）、
> [`FEATURE-GAPS.md`](./FEATURE-GAPS.md)（功能规划）。

---

## 0. 一句话状态

冷导入的成本几乎全在「解码 + 写缩略图」：`stage` 占导入总时间 **96–98%**，`commit` 只占 **2–4%**。
已经吃掉的量级收益有两个 —— **池宽自适应**（大图 4→12 线程 **1.85×**）和**解码只做一次**（缓存重导入 ~20%）。
剩下的分三类：**三个没标定的常数**、**每事务 fsync 与索引 drain 的位置**、**解码实现本身**。

---

## 1. 现在的形状

### 1.1 两个阶段

| 阶段 | 在哪 | 干什么 |
|---|---|---|
| stage | 后台线程（`media/import.rs::stage_all`） | hash + probe + 解码 + 缩略图 + 调色板 + 签名，纯文件系统工作 |
| commit | 导入 job 线程（`commit_staged_all`） | 去重 / 插入 / 挂合集，`COMMIT_BATCH = 16` 文件一事务（`tasks/import.rs:39`） |

- **分窗口**：`stage_window()`（`tasks/import.rs:51`）= 地板池宽 × `COMMIT_BATCH` × 2 = **128 文件**。窗口让内存与取消延迟不随批次规模增长（10 万文件拖进来也能中途取消）。
- **目录展开在 job 线程**（`expand_dirs`），UI 只 `stat` 判断有没有目录。
- UI 线程不参与：DB 是主线程独占的，stage/commit 各用后台线程与自己的连接。

### 1.2 六个 stage（`media/pipeline.rs::default_pipeline`）

| stage | 产出 | 说明 |
|---|---|---|
| `hash` | `Need::Hash` | 链接导入只哈希原文件（256 KiB 缓冲）；copy 模式边复制边哈希 |
| `probe` | `Need::Probe` | 扩展名 → kind/mime；图片读头部取尺寸；mp4 走 moov，其它容器走 ffprobe |
| `decode` | `Need::Decode` | **唯一的解码**，产物 `Decoded` 进 artifact 缓存 |
| `thumb` | `Need::Thumb` | 从共享解码缩放 + 写 JPEG（temp + rename，**不 fsync**） |
| `mine` | — | EXIF / 调色板来自共享解码；时长从 probe 阶段带过来 |
| `visual-sig` | — | pHash + 颜色直方图，同样读共享解码 |

- **RAW / HEIF / AVIF 的「读头」本身就是整解码** → `probe` 阶段跳过它们，decode 阶段是唯一读者（`probe::dimensions_need_full_decode`）。
- **命中缩略图缓存时，decode 解的是缩略图而不是原图**（`DecodeStage` 注释）：这是热重导入 0.89 ms/文件 的原因。

### 1.3 线程宽度：按批选臂（不是固定值）

| 臂 | 宽度 | 何时选 |
|---|---|---|
| 窄 | 4 | 抽样（最多 64 个源，带 stride）的平均字节 < 64 KiB |
| 宽 | 12 | 平均 ≥ 64 KiB |

`TROVE_STAGE_THREADS` 覆盖双臂；`stage_thread_count_for(&paths)` 报该批实际宽度。
**为什么不是固定 4**：大图是 CPU 受限、小图是元数据受限（见 §2.1）。

### 1.4 子进程闸（`media/proc.rs`）

`MAX_SLOTS = 4`，`TROVE_PROC_SLOTS` 覆盖。导入期的三处子进程取槽：视频缩略图（ffmpeg）、非 mp4 事实（ffprobe）、HEIF/AVIF（heif-dec）。
**播放管道故意不取槽**（用户主动发起，至多一两个）。加它的原因：宽池 12 会让一批视频同时起 12 个解码进程。

### 1.5 采集与目录监视（同在导入路径上）

- **collect**（`services/collect.rs`）：落盘 `.part` → `sync_all` → 写 sidecar → `rename`；`inbox_items_in` 跳过 `.part`；固定 4 worker + 有界队列 + 60 s 空闲超时；body 流式落盘（512 MB 上传不再等于 512 MB 内存）。
- **watch**（`tasks/watch.rs`）：notify 事件（`TICK = 400 ms`）+ 5 s 全扫兜底（watcher 覆盖全部 root 时退避 12× → 60 s）+ `SETTLE = 1500 ms`（内核在 `open` 时就报事件）+ **ack 回执**（未 ack 的文件继续重试，ack 过的停止上报）。inbox 信号是**边沿触发**的（与上次信号时的列表比较），导入 job 再按（文件名, 大小）把已在库中的历史文件挡掉——两层合起来，长期存在的 inbox 才不会变成常驻的重哈希循环。

---

## 2. 已量到的数

> 全部来自**真实终端**（脚本开头 sanity 行通过：新建文件 0.0135 ms/个）。执行器沙箱里这些数字无效，见 §6。

### 2.1 池宽扫描（ms/文件，5 轮 × 3 pass 的中位数）

| 源 | w1 | w2 | w4 | w6 | w8 | w12 |
|---|---|---|---|---|---|---|
| 30 × 3000x2000 JPEG（≈290 KB/个） | 30.63 | 16.74 | 9.19 | 7.10 | 6.36 | **4.98** |
| 300 × 1x1 PNG（≈70 B/个） | 0.0445 | 0.0265 | **0.0190** | 0.0195 | 0.0200 | 0.0210 |

- 大图：w1 的 30 ms 里 **≥97% 是 CPU**（解码/缩放/编码），I/O 只值 <0.2 ms → 一路降到 12，**8→12 还有 1.28×，12 处仍未到平台**。
- 小图：0.019 ms/文件已贴着「新建文件」地板（0.0135 ms）→ 4 线程即平台，更宽只加深元数据争用（w12 慢 10%，但 300 个文件全批只差 0.6 ms）。
- 判据（可复用）：**w1 的每文件耗时 ÷ 单文件写盘成本**。≫1 → CPU 受限，加宽有效；≈1 → 元数据受限，加宽无益。

### 2.2 一次导入的组成（30 个大图，pool = 4）

| 指标 | 值 |
|---|---|
| `stage_all` 冷跑 | 262.5 ms / 30 文件 = **8.75 ms/文件** |
| `commit` | 10.4 ms = **4%** |
| `stage_all` 第二遍（缩略图已缓存） | **0.89 ms/文件**（约 10× 便宜） |

### 2.3 并行效率

大图 w12 / w1 = **6.15×**，效率 51%（本机 6P+8E 混核 + 尾效应；30 个文件 12 线程 ≈ 2.5 波）。

---

## 3. 已经做过的优化（别重复提）

| 批次 | 提交 | 内容 | 实测 |
|---|---|---|---|
| 批 1 | `46fc363` `244208c` `13778d5` `e897198` | 六阶段管线 + 一次解码 + 分窗口 + 目录展开移出 UI | 纯图片冷导入 16.4 → 16.2 ms/文件（噪声内）；缓存重导入 −20% |
| 批 1 | 同上 | RAW/HEIF/AVIF 从两次整解码 → 一次 | 未单独量，结构收益 |
| 批 2 | `51843e0` | 池宽按批平均源大小选臂（4 / 12，阈值 64 KiB） | 大图 **1.85×** |
| 批 3 | `ad1f60f` | collect：`.part`+rename+先写 sidecar、worker 池、流式落盘 | 正确性（半写文件曾被导入） |
| 批 3 | `193b3a3` `d966ab0` `292b836` | watch：notify 事件 + 兜底 sweep + **ack 回执**（修掉「每次 sweep 重导入」的无限循环） | 新文件 ≤400 ms 可见；不再无限重哈希 |
| 批 4 | `2c3495c` `e72e5e2` | 视频类型表唯一化、非 mp4 容器走 ffprobe、`Cost::Proc` 真正落地 | 正确性 + 防止 12 个并发解码器 |
| 批 5 | 2026-09-19 | **inbox 正确性三连修**：信号改「边沿触发」（比较上次列表，不再每 5 s 对非空 inbox 发信号）；job 内按（文件名, 大小）去重，历史文件计入 `already_imported` 不再重哈希；列举统一走 `inbox_items_in`（此前 `collect_inbox_app` 手写列举漏掉 `.part`，半写文件会被永久导入；`.trove.json` 旁注也被当素材导入） | 正确性；运行时验证见 git |
| 批 5 | 同上 | commit 批级错误不再丢报告：批失败整体回滚并逐文件记入 skipped，`ImportOutcome.error` 带首错上抛 UI；`stamp_collect_source` O(n²) → HashMap；符号链接/非 UTF-8 名记入 skipped；未来 mtime 视为已 settle | 正确性（有测试） |

**基准工具**（都随代码一起维护）：`examples/stage_sweep.rs`（池宽臂跑器）、`examples/import_profile.rs`（组件归因）、`examples/event_path_profile.rs`（锁/事件路径）、`examples/commit_batch_sample.rs`（COMMIT_BATCH 采样，走真实任务路径）。

---

## 4. 还能做的（按 ROI 排序）

### P0 · 标定三个常数（纯测量，不需要写代码）

| 常数 | 现值 | 依据 | 怎么定 |
|---|---|---|---|
| 宽池上限 | **12** | 扫描到 12 时仍在降 | `bench-real.sh` 的 16/20 臂 |
| 选臂阈值 | **64 KiB** | **插值**，未实测 | sm(≈23 KB) / mid(≈134 KB) 两档夹；结论不同就在中间插一档 |
| 进程闸 | **4** | 按机器拍的 | `vid` 档 + `TROVE_PROC_SLOTS=1/2/4/8` |

这是当前 ROI 最高的一步 —— 上面其它结论都建立在「池宽/闸宽选对了」之上。

### P0 · SQLite 每次 commit 都在 fsync —— ✅ 已做（2026-09-19）

- `synchronous = NORMAL`（store 与导入 job 两条连接都设）+ `cache_size = -16000`。WAL 的常规配对：应用崩溃仍安全（WAL 重放），只有断电可能丢最后一个事务，而导入本来可重跑。
- **标定数据**（1000 × 1×1 PNG，真实终端，5 轮丢首轮取中位）：

  | TROVE_COMMIT_BATCH | ms/文件（中位） |
  |---|---|
  | 16 | 0.033 |
  | **64** | **0.0285** |
  | 128 | 0.0285 |
  | 256 | 0.0285 |

  结论：NORMAL 落地后 fsync 不再按事务付，批大小过 64 即平台；64 比 16 快 ~16%，再大只是持锁更久、进度更粗。**默认 64**（`tasks/import.rs::commit_batch`），`TROVE_COMMIT_BATCH` 可覆盖。采样工具：`examples/commit_batch_sample.rs`（走真实 `tasks::import::run`，process-per-sample，同 stage_sweep 惯例）。

### P1 · 索引 drain 落在「用户第一次搜索」上

- `search_assets`（`library.rs:447`）与 app 栅格刷新（`trove-app/.../data.rs:276`）都在 **UI 线程**先 `drain_search_queue()`；**导入 job 自己不 drain**（`search.rs:592` 的 `DRAIN_BATCH = 8000`）。
- 后果：导入 1000 个文件 → outbox 留 1000 条 → 用户导入完**搜一下**时在 UI 线程一次性索引 + Tantivy commit（fsync）。空队列时确实便宜（一次小 SELECT），问题在非空那一次。
- 改法：导入 job 在自己的线程上按窗口 drain（每 N 个窗口一次），把成本挪出用户可见路径。**这是延迟（能感觉到）而非吞吐问题。**

### P1 · 大图是「整解码 → 缩小」

- `thumb::ensure` 全解码再缩到 512。libjpeg-turbo 的 **DCT scaling**（直接解 1/2、1/4、1/8）能省掉大部分解码工作，对 24 Mpx 照片理论 2–3×。
- 代价：`image` crate 不做这件事，要走 `mozjpeg` / `jpeg-decoder` 的缩放路径 + 分格式分支。
- **先看 `import_profile` 组件行**里 thumb 的绝对值与其中 decode 的比例，≥60% 才值得为它引依赖。

### P2 · 视频的哈希成本

- `blob::hash_file` 是整文件读 + sha256：4 GB 视频光哈希就是秒级。
- **不要在这里重复设计** —— `MODEL-PREVIEW-BACKLOG.md` 的 **P8「导入延迟哈希」** 已有完整分析（sha256 同时是 blob 名/缩略图名/去重键/回收判据，四个键一起动）并等拍板。
- 本文只补一条：**同一个决策里可以顺带讨论换 blake3**（快 4–8×），代价同样是那四个键全部改语义 —— **0.5 发布前是唯一窗口**，发布后就不能改了。不建议采样/分块哈希（破坏去重与完整性校验）。

### P2 · 视频每文件两趟子进程

ffprobe 取事实 + ffmpeg 取帧，闸宽 4 未标定。ffmpeg 已经是 input seeking（`-ss` 在 `-i` 之前），还能加 `-skip_frame nokey`；真正该做的是随 P0 把闸宽扫一遍。

---

## 5. 已到顶 / 不要再动

| 项 | 为什么不划算 |
|---|---|
| 再提高线程宽度 | 大图 w12 已是 w1 的 6.15×，效率 51%（混核 + 尾效应）；更宽要拆缩放/编码到独立池，收益与复杂度不成比例 |
| hash 与 decode 各读一遍文件 | 页缓存通常吸收第二遍；只有 GB 级视频例外（见 P2） |
| 缩略图 temp+rename 加 fsync | 故意的，加了每条 thumbnail 都多一次 fsync，收益为零（坏了重建即可） |
| 每文件 progress 上报 | 37.7 ns/call，约文件预算的 0.0004%（`event_path_profile` 实测） |
| `sample_avg_bytes` 每批 64 次 stat | 一次 `stat` 1.3 µs，整批 <0.1 ms |
| 采样/分块哈希 | 破坏去重与完整性语义，不做 |

---

## 6. 怎么测（铁律 + 命令）

**铁律一：必须在真实终端跑。** 执行器沙箱把「新建文件」串行化到 ~21 ms/个（真实 0.0135 ms），而导入每个文件都要写一张缩略图 → 曲线被完全压平，
CPU 侧优化被掩盖。这条踩过两次（一次把池宽结论做反，一次采到过一张脏表）。
`target/tmp/bench-real.sh` 开头的 sanity 行是**结论有效性的唯一凭据**：显示 >1 ms/个就整张表作废。

**铁律二：测试与基准都要 `TMPDIR=/home/noke/Code/trove/target/tmp`**（`/tmp` 只有 10 MB tmpfs）。

```bash
cd ~/Code/trove
bash target/tmp/bench-real.sh                      # 全套：四档源 × 1..20 臂 + 自适应臂 + vid 子进程闸扫描

# 单臂（手动钉宽度或闸宽）
TROVE_STAGE_THREADS=12 target/release/examples/stage_sweep target/tmp/prof-src 30 5
TROVE_STAGE_THREADS=12 TROVE_PROC_SLOTS=2 target/release/examples/stage_sweep target/tmp/vid-src 16 3

# 组件归因（stage / commit / 每步单独耗时 / 热重导入对比）
cargo run --release -p trove-core --example import_profile -- target/tmp/prof-src 30 3

# 搜索查询路径（含 drain 计时）
TROVE_PROFILE_QUERY=1 <运行应用或测试>
```

采样约定：`stage_sweep` 是 **process-per-sample**（池在进程内只建一次），首轮 pass 略慢 → 丢 `pass=0`，取 pass 1..2 的中位数；
**按轮次交错跑臂**（库文件系统在不同坐次之间会漂几倍）；只信同一坐次的 A/B。

---

## 7. 未标定常数一览（一眼看清欠什么）

| 常数 | 位置 | 现值 | 状态 |
|---|---|---|---|
| `STAGE_THREADS_NARROW` | `media/import.rs` | 4 | 有大图/小图两组实测支撑 |
| `STAGE_THREADS_WIDE_MAX` | `media/import.rs` | 12 | **上限未定**（12 处仍降） |
| `STAGE_WIDE_MIN_AVG_BYTES` | `media/import.rs` | 64 KiB | **插值**，未实测 |
| `MAX_SLOTS` | `media/proc.rs` | 4 | **未实测** |
| `COMMIT_BATCH` | `tasks/import.rs` | 64 | ✅ 已标定（见 §4；16→64 快 ~16%，64/128/256 持平） |
| `TICK` / `SETTLE` / `SWEEP_COVERED_FACTOR` | `tasks/watch.rs` | 400 ms / 1500 ms / 12× | 体验驱动，未做 A/B |

---

## 8. 交接：如果只做一件事

跑一次 `bench-real.sh`，把 §7 表里前三行的状态从「未实测」改成实测值。
在此之前，任何「再优化导入」的讨论都建立在没标定的常数上。
