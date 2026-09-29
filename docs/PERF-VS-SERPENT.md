# Trove ↔ Serpent 性能对照

> 对照物：`reference/Serpent`（上游 [dolag233/Serpent](https://github.com/dolag233/Serpent)）**v0.2.9**，Electron 43 + React 19 + better-sqlite3(FTS5)。
> 被测方：Trove **v0.4.9**，Rust + GPUI（wgpu）+ rusqlite(bundled SQLite) + Tantivy。
> 核验日期 **2026-09-27**。功能差距见 [GAP-TO-SERPENT.md](./GAP-TO-SERPENT.md)，本文只谈速度。
>
> **结论先说。** 同一台机器、同一批 2 万 / 10 万个素材、同一组问题：查询层 20 项配对指标
> Trove 领先 18 项（其中侧栏文件夹 1.08× 算打平），而且赢面随库规模变大（`filterRatingMs`
> 从 19.6× 涨到 30×，`searchFixedTokenMs` 从 6.5× 涨到 12.5×）；落后的两项
> `collectionSwitchMs`（0.46×）与 `folderSwitchMs`（0.09×）加起来指向同一件事——**Trove 每次切
> 视图都要算一次精确总数，而那条 COUNT 无条件挂着 20 万个标量子查询**，把 0.17 ms 的列表抬到
> 3.99 ms（20k）/ 26.9 ms（100k）。进程层是量级差：出窗 559 ms 对 1543 ms，常驻 PSS 258 MB 对
> 572 MB，1 个进程对 8 个。

## 怎么测的，怎么重跑

```console
bash bench/run-all.sh                  # fixture → serpent → trove → proc → aggregate
node bench/aggregate.mjs               # 只重画表格（bench/results/summary.md）
```

三层各用一套办法：

**查询与浏览层。** 两边跑**同一份 fixture**：Serpent 自带的 `large-library-fixture` 生成器
（v4，seed 20260816，mixed profile，91 % 图片 + 5 % 视频/模型/文本/音频/不支持，160 个文件夹、
50 个合集、7 个标签，8K/4K/2K/1K 四档分辨率按 1/3/30/66 % 分布）。20 000 资产的树 = 29 GB 真实
文件、库数据库 84.5 MB；100 000 资产的树 = 143 GB、库数据库 421.7 MB。Serpent 侧跑**它自己的**
`tests/worker/comprehensive-perf-bench.test.ts` 与 `large-library-performance.test.ts`（一条命令
没改），Trove 侧跑 `crates/trove-core/examples/serpent_parity_bench.rs`，指标名与 Serpent 的
JSON 键逐一对齐。

> **为什么 Trove 的查询层不吃那棵文件树。** fixture 的图片和视频是**池化复制**的：2 万个文件
> 只有 45 份唯一内容。Serpent 的库是生成器**直接写行**建起来的，2 万行照单全收；Trove 的导入
> 按 BLAKE3 内容哈希去重，同一棵树只留下 741 个资产（见 §导入层）。用 741 行的库去回答 2 万行
> 的查询问题，得到的是假数字。所以 Trove 的查询层比的是**镜像库**：把 Serpent 那份
> `library.db` 的每一行翻译成 Trove 的 `Asset`（同名、同体积、同宽高、同评分/标签/合集成员/
> 时间戳）直接插入——而"直接写行、不走导入"恰好就是 Serpent 自己构造基准数据的方式，两边同题。
> 20k 镜像库的建库成本是：3.07 s 插 2 万行、4.95 s 挂 59 200 条合集成员与 41 177 条标签关系、
2.02 s 建全文索引。

**进程层。** 一个外部探测器，两边同用：`niri msg --json windows`（单次查询实测 6.6 ms）报出
窗口的时刻算"出窗"，`/proc` 采样进程树的 RSS / PSS / CPU / 线程，静止后再停留 5 s 取末值。
不从任何一个 app 内部取数，也不要求谁开放钩子。

**导入层。** 同一棵 `Assets/` 树喂给 Trove 的真实导入任务；Serpent 侧的对应测量在本机跑不起来，
原因写在 §导入层。

## 机器与三条读数前提

| | |
|---|---|
| CPU | 12th Gen Intel i7-12700H（20 线程，4.7 GHz 上限） |
| 内存 | 15 GiB，zram 交换 15 GiB |
| 系统盘 | Micron 2450 NVMe 512 GB，**btrfs**，测量期间 54 % 占用 |
| GPU | Intel Iris Xe（核显，桌面合成走它）+ RTX 3050 Mobile |
| 内核 / 桌面 | Arch，7.2.7-zen，niri（Wayland） |
| 运行时 | rustc / cargo 1.98.0；Electron 43 自带 Node 24.18.0 |
| Trove 构建 | `cargo build --release`（`strip = true`，`lto = true` + `codegen-units = 1`，即 fat LTO） |

1. **每个数字是「1 次预热 + N 次计时」的中位数，min/max 一起给**（查询层 N=5，Serpent 侧重复
   3 轮）。这台机器的文件系统是 btrfs，`examples/import_bench.rs` 的模块注释早就写明单次测量
   能漂 2–3 倍。
2. **测量严格串行。** 前后来回踩过两次：两个 app 的 bench 同时跑时，双方数字一起虚高 20–30 %，
   且 min/max 收得很紧、看起来极其可信——第一版 20k 数据就是这么废掉的。最终这轮一个等一个。
3. **Serpent 的 GUI 在本机必须走 XWayland。** 默认 ozone/Wayland 后端下它建了 surface 却永远不
   map 窗口（停在 `show:false` 等 `ready-to-show`，日志里只有几条 `wayland_surface.cc` 能力
   警告），加 `--ozone-platform=x11` 才出窗。进程层那三个 Serpent 数字因此都含 XWayland 转换
   的成本。fixture 生成还有另一段弯路（sharp 在 `ELECTRON_RUN_AS_NODE` 下段错误），记在
   `bench/serpent/README.md`。

## 口径：哪些是真同题，哪些不是

写在表格前面，因为表格会把这层抹平。

- **两边都含精确总数。** Serpent 的 `searchAssets` 会跑一条 COUNT（它源码里的注释写着
  "COUNT query and the data query therefore both scanned…"），Trove 的 `BrowseContext::run()`
  是 `snapshot(count=true) + page`。所以配对表里的首页 / 切合集 / 切文件夹三行是同题的。
  Trove 另有一列 `*NoCountMs`（走 `run_without_count`，也就是网格刷新时真正走的那条路），
  Serpent 没有对应物，不参与比值。
- **排序方向两边都是"最新在前"。** 这一条差点把整份报告带偏：Trove 的 `AssetQuery::default()`
  里 `sort_desc` 是个裸 `bool`、默认**升序**，而库里的有序偏索引建的是 `(col DESC, id ASC)`，
  升序带升序 tiebreaker 时 SQLite 用不了它（`EXPLAIN` 给 `USE TEMP B-TREE`）。第一版 bench 因此
  把首页量成 18 ms，实际 0.15 ms。现在两边都显式倒序；升序那一档作为独立指标留在 Trove 侧
  （`browseFirstPageAscMs`：20k 17.0 ms / 100k 98.8 ms），它是"少了反向偏索引"的代价实测。
- **`searchFixedTokenMs` 那行不完全同题。** fixture 里 2 万条描述都含 `asset`，而 Trove 的检索
  候选受 `CANDIDATE_CAP = 2000`（`crates/trove-core/src/search.rs:65`）截断，Serpent 的检索
  路径里没读到对应上限。真正同题的是选择性检索那一行：`serpent-large-library-needle`，
  命中 1826 行，两边都在各自上限内（Serpent 的 `needle` 由 `bench/serpent/search-probe.ts`
  量得，它调的就是 Serpent 自己的 `LibraryService.searchAssets`）。
- **`sidebarListFoldersMs` 量的不是同一件事。** Serpent 列出 160 行文件夹记录（18.8 ms 是它
  自己的量级），Trove 侧栏没有文件夹表，`assets::source_folders` 要把 2 万行
  `json_extract(extra,'$.source_path')` 现场读出来归并（17.5 ms @20k，89.9 ms @100k）。
  这一格属于 GAP 文档里"没有磁盘文件夹树"那个结构性洞，不是查询慢。
- **`collectionRecursiveSwitchMs` ↔ `folderSwitchRecursiveMs` 是松配对。** Serpent 问的是递归
  合集，Trove 没有递归合集，取的是子树文件夹前缀。两列并排放着，但别当成同题。

## 查询与浏览层 · 20 000 资产

ms，越小越好；倍数 = Serpent ÷ Trove。

| 指标 | Serpent | Trove | 倍数 | 口径 |
|---|---:|---:|---:|---|
| `openLibraryMs` 打开库（冷） | 7.60 | 1.74 | 4.37× | 冷打开同一个库数据库 |
| `allBrowseFirstPageMs` 默认浏览首页 50 行 | 15.80 | 3.99 | 3.96× | 两边都含精确总数 |
| `deepOffsetPageMs` 深翻页 offset 10000 | 31.35 | 5.25 | 5.97× | |
| `collectionSwitchMs` 切进合集（1184 成员） | 7.20 | 15.76 | **0.46×** | Trove 输 |
| `folderSwitchMs` 切进文件夹（一层） | 1.41 | 15.12 | **0.09×** | Trove 输；Serpent 走外键，Trove 走 `json_extract LIKE` |
| `collectionRecursiveSwitchMs` ↔ 子树文件夹 | 64.30 | 9.76 | 6.59× | 松配对 |
| `searchFixedTokenMs` 高频词 asset | 86.55 | 13.29 | 6.51× | 见口径：Trove 有候选上限 |
| `needle` ↔ `searchNeedleMs` 选择性检索 | 23.82 | 8.73 | 2.73× | 真同题（两边都在上限内） |
| `layoutOnlyMs` 整表瀑布流几何 | 152.07 | 55.18 | 2.76× | |
| `sortNameAscMs` 按名称 | 15.85 | 3.88 | 4.09× | |
| `sortCreatedAtDescMs` 按创建时间倒序 | 15.65 | 3.99 | 3.92× | |
| `sortModifiedDescMs` ↔ `sortUpdatedAtDescMs` | 57.82 | 9.36 | 6.18× | 两边都无有序索引 |
| `sortByteSizeDescMs` 按体积倒序 | 56.20 | 3.93 | 14.30× | |
| `sortRatingDescMs` 按评分倒序 | 59.53 | 3.84 | 15.50× | |
| `filterRatingMs` 筛选评分 ≥3 | 35.37 | 1.80 | 19.65× | |
| `inspectorMetadataMs` 检查器一次读 | 0.15 | 0.05 | 3.00× | |
| `sidebarListFoldersMs` 侧栏文件夹 | 18.84 | 17.52 | **1.08×** | 量的不是同一件事 |
| `sidebarListCollectionsMs` 侧栏合集 | 0.24 | 0.04 | 6.00× | |
| `browseSessionOpenMs` 冻结分页会话 | 16.20 | 3.57 | 4.54× | |
| `browseSessionPageMs` 会话内取一窗 | 16.40 | 5.51 | 2.98× | |

**读法。** Trove 的列表本身几乎没有成本：`allBrowseFirstPageNoCountMs` **0.17 ms**、
`collectionSwitchNoCountMs` **0.51 ms**、`deepOffsetPageNoCountMs` 1.57 ms。取第 10 000 行往后
的 50 行只比取第一页贵 1.4 ms（bench 里手写的同一条 SQL 实测 0.25 ms），因为
`(created_at DESC, id ASC) WHERE trashed_at IS NULL` 让 SQLite 纯走索引跳过，不回表。Serpent 侧的 `browseSessionPageMs` 是 16.40 ms，
与它的 `allBrowseFirstPageMs`（15.80）基本相等，说明那 15 ms 是每次取窗都重付的固定成本。
20 项配对指标里 Trove 领先 18 项：落后的两项 `collectionSwitchMs` / `folderSwitchMs` 与基本
打平的 `sidebarListFoldersMs` 都是"切视图 / 侧栏刷新 + 一次全表统计"这一族，见 §Trove 输在哪里。

## 查询与浏览层 · 100 000 资产

| 指标 | Serpent | Trove | 倍数 |
|---|---:|---:|---:|
| `openLibraryMs` | 8.20 | 2.25 | 3.64× |
| `allBrowseFirstPageMs` | 113.01 | 26.93 | 4.20× |
| `deepOffsetPageMs` | 137.93 | 28.28 | 4.88× |
| `collectionSwitchMs` | 35.25 | 172.73 | **0.20×** |
| `folderSwitchMs` | 2.09 | 66.45 | **0.03×** |
| 子树文件夹 ↔ `collectionRecursiveSwitchMs` | 420.10 | 50.78 | 8.27× |
| `searchFixedTokenMs` | 539.50 | 43.04 | 12.53× |
| `needle` ↔ `searchNeedleMs` | — | 17.46 | 未测 100k |
| `layoutOnlyMs` | 625.95 | 399.36 | 1.57× |
| `sortNameAscMs` | 112.35 | 27.20 | 4.13× |
| `sortCreatedAtDescMs` | 110.96 | 27.45 | 4.04× |
| `sortModifiedDescMs` | 405.24 | 58.34 | 6.95× |
| `sortByteSizeDescMs` | 418.36 | 26.32 | 15.90× |
| `sortRatingDescMs` | 430.98 | 26.44 | 16.30× |
| `filterRatingMs` | 241.35 | 8.03 | 30.06× |
| `sidebarListFoldersMs` | 109.19 | 89.93 | 1.21× |
| `sidebarListCollectionsMs` | 0.22 | 0.04 | 5.50× |

**读法：两边都在 5 倍规模上超线性变慢，但 Trove 的超线性集中在一项上，能指出来。**
Serpent 从 20k 到 100k：首页 15.8 → 113（×7.2）、`filterRatingMs` 35.4 → 241（×6.8）、
`searchFixedTokenMs` 86.6 → 540（×6.2）——每一项都在涨，从数字上看不出结构。
Trove 从 20k 到 100k：首页 3.99 → 26.9（×6.7）看着同样糟，拆开就清楚了——列表本身
0.17 → **0.16 ms（一点没涨）**，全部涨幅都在 `exactCountOnlyMs` 3.60 → 26.35 ms。
扣掉这一项之后，Trove 只有一处涨得比 Serpent 快：`layoutOnlyMs` 55.2 → 399.4（×7.2）对
Serpent 的 152.1 → 626.0（×4.1），领先从 2.76× 缩到 1.57×。这一项的量不是几何计算而是
**把整表读成 `Asset` 结构**：bench 里 50 行的同一条读路径是 0.16 ms（≈3 µs/行），20k 行
外推 60 ms、100k 行外推 300 ms，与实测的 55 / 399 对得上——超线性那 100 ms 来自分配与
缓存压力，不是 `justify_layout` 的算法段（`layout.rs:85` 的 `EXACT_DP_THRESHOLD = 500` 以上
是 O(n) 并行贪心）。

## 进程层

三个数各测 3 次取中位数；`Empty` 是空配置文件（没有库）的同一套测量。

| 指标 | Serpent | SerpentEmpty | Trove | TroveEmpty |
|---|---:|---:|---:|---:|
| 出窗 spawn → 窗口 map (ms) | 1543 | 1349 | 559 | 559 |
| 静止 CPU 安静 1 s (ms) | 3046 | 2243 | 660 | 607 |
| 启动期 CPU 时间 (ms) | 2800 | 3010 | 610 | 450 |
| 进程数 | 8 | 8 | 1 | 1 |
| 线程数 | 84 | 104 | 50 | 50 |
| 常驻集 RSS (MB) | 908.8 | 1008.4 | 293.1 | 267.8 |
| 比例常驻 PSS (MB) | 572.3 | 590.6 | 257.9 | 232.4 |
| 峰值 PSS (MB) | 778.5 | 609.1 | 260.7 | 232.4 |

**读法。**
1. **出窗 2.8×、静止 4.6×、CPU 4.6×。** Trove 空库与带 2 万资产库的出窗时间一模一样
   （559 ms 对 559 ms），静止时间只差 53 ms——那 559 ms 是运行时自身的准备成本（进程加载、
   渲染后端与首帧），与库里有多少行无关。Serpent 的对应差值是 1543 − 1349 = 194 ms，
   也就是说它的开库成本同样很小，慢的是壳。
2. **Serpent 的常驻内存几乎全是 Electron 壳，不是数据。** 空壳 PSS 590.6 MB，开着 2 万资产库
   PSS 572.3 MB——库开不开在噪声范围内。Trove 是 232.4 → 257.9 MB，2 万行数据只加 25 MB。
   比值 2.3× 就是这么来的，而且这个比值**不会因为库变大而改变**：它量的是运行时，不是数据。
3. `Serpent` 那行的峰值 PSS 778.5 MB 比空壳高 170 MB，那是打开库时 worker 建 FTS 与快照的瞬时
   开销；Trove 的对应峰值只比稳态高 2.8 MB。
4. 线程数反向：Trove 50，Serpent 空壳 104。单进程的代价是线程给得多，不是省。

**产物体积**（各按自己发行方式的实测字节）：

| | |
|---|---|
| Trove | `trove-app` 80.3 MiB 单一可执行文件（`strip`、fat LTO（`lto = true` + `codegen-units = 1`），运行时链接 15 个系统库，语言包由 `rust_i18n::i18n!` 编译期内嵌）+ `trove` CLI 34.1 MiB |
| Serpent | Electron 运行时 312 MiB + 构建产物 31.1 MiB（`.vite/build` 8.1 + `.vite/renderer` 23.0）+ 原生模块 ≈ 36.4 MiB（better-sqlite3 18、sharp-libvips 18、sharp 0.4）+ ufbx 1.7 + fonts 0.7 ≈ **383 MiB**，不含安装器与账号侧另取的 ffmpeg 二进制 |

两边都是 `du` 的 MiB 口径。体积比 4.8×，注意 Serpent 那侧含整个 Chromium；它的 Windows
安装器会压掉很大一部分，所以这一条只能当数量级看，不能当"发布体积"看。

## 导入层

同一棵 `Assets/`（20 000 文件 / 28.3 GiB）交给 Trove 的真实导入任务：

```text
  import          : 73.7 s   (271 files/s, 394 MiB/s)
  outcome         : 741 inserted, 19259 deduped onto an existing row, 0 skipped
  thumbnails      : 445
  text index      : 0.0 s   (741 docs, 已在导入中建好)
  library on disk : 11.9 MiB
```

**这层没有可信的对方数字，两个原因叠在一起。**

1. **语义不同，不是快慢不同。** fixture 的 2 万个文件只有 45 份唯一内容，而 Serpent 的库是
   生成器直接写行建的（2 万行照单全收）。Trove 按内容哈希去重，同一棵树收敛成 741 个资产——
   这是产品语义分歧（GAP 文档 §H 的"只链接、不托管"背后同一条取舍），不是导入慢。把它写成
   "Trove 用 73.7 s 处理了 Serpent 用 …" 会是这份报告里最假的一行。
2. **Serpent 自带的摄取基准在本机跑不起来。** `tests/worker/media-task-performance.test.ts`
   （50–200 个资产重生成缩略图、带 RSS 与事件循环延迟采样）在 Linux 上先要 `cp -a` 整份 29 GB
   fixture，`execFileSync` 抛 `ENOBUFS`；改用它的 `SERPENT_MEDIA_TASK_PERF_REUSE_LIBRARY`
   跳过克隆后，worker 在真正调用 sharp 生成缩略图时退出——和 fixture 生成阶段那个
   `ELECTRON_RUN_AS_NODE` 下的 sharp 段错误是同一处（`bench/serpent/README.md` 里有最小复现）。
   它的 `large-library-thumbnail-performance.test.ts` 同理。

所以导入层本文只报 Trove 的绝对值：28.3 GiB 的树读完并入库、741 个资产（图片与视频为主）落了
445 张缩略图，用时 73.7 s，即 394 MiB/s、271 文件/s。这组数只说明"这棵树在这个设备上被完整
处理了一遍"，**不能当解码吞吐用**——20 000 个文件里只有 45 份唯一内容，哈希与去重承担了大部分
工作，解码只发生一次。逐阶段的成本属于 `crates/trove-core/examples/import_profile.rs`。

## Trove 输在哪里，各值多少

每一项落后都能拆成"列表"与"总数"两段，列表那段实测，总数那段用差值算出来：

| 落后项 | 20k 总计 | 其中列表（`*NoCount` 实测） | 推断的总数成本 | 100k 总计 | 其中列表 |
|---|---:|---:|---:|---:|---:|
| `allBrowseFirstPageMs` | 3.99 | 0.17 | ≈3.8（直接量得 3.60） | 26.93 | 0.16 |
| `collectionSwitchMs` | 15.76 | 0.51 | ≈15.3 | 172.73 | 0.56 |
| `folderSwitchMs` | 15.12 | 6.76 | ≈8.4 | 66.45 | 14.63 |

**默认浏览这一行是干净的：`exactCountOnlyMs` 3.60 ms 对 `countFloorMs` 0.40 ms（100k：26.35 对
1.99）。** 两条 COUNT 在 bench 里同进程、同连接、同 pragma、语句缓存同样热，差别只在 SQL 文本：
`build_where` 给每一条 live 查询无条件附加了序列帧隐藏子句——

```sql
NOT EXISTS (SELECT 1 FROM asset_sequence_frames f
            WHERE f.asset_id = assets.id AND f.position > 0)
```

（`crates/trove-core/src/store/sequences.rs:28`，挂接点在 `store/assets.rs:904`。）
`asset_sequence_frames.asset_id` 是 UNIQUE，所以它是每行一次索引探针，库里一个序列都没有也照付；
20k 值 3.2 ms、100k 值 24.4 ms，随行数线性——量级和"20 万次探针"对得上，与算法无关。
其余被排除的可能都留在 bench 的输出里：行映射（24 列全读 + `extra` 反序列化成 `AssetFacts`，
50 行 0.16 ms 对手写 0.09 ms）、连接与 pragma（同一份 pragma 手写 0.06 ms）、JSON 体积
（`extra` 平均 76 B/行，100k 时 387 B/行）、WAL（测量时 `-wal` 为 0 字节）。

**合集与文件夹两行只能拆到"总数占大头"这一步，不能再往下列原因。** 切进合集的总数成本从
15.3 ms（20k）涨到 172 ms（100k），×11 对 ×5 的行数——比每行一次探针的线性预期陡，说明
`EXISTS (… asset_collection …)` 与序列帧子句在 100k 上互相放大了什么（驱动顺序？统计信息？）。
本文只报实测，不给结论；`EXPLAIN QUERY PLAN` 单看是理想的（`SCAN assets USING
idx_assets_live_created` + `SEARCH ac EXISTS USING COVERING INDEX`）。

如果只改一处，改默认浏览那一条：序列帧表为空时省掉子句，或把"是否序列成员"落成一个可索引的
列。20k 首页会从 3.99 掉到 1 ms 量级、100k 从 26.9 掉到 4 ms 量级——但**不会**因此反超
`collectionSwitchMs`（Serpent 7.20 ms）：那一行还得先解决 ×11 的总数增长。
`folderSwitchMs` 是另一件事：Trove 没有文件夹表，`json_extract(extra,'$.source_path') LIKE
'prefix%'` 用不了任何索引，列表段自己就要 6.76 ms（20k）/ 14.63 ms（100k），而 Serpent 走的是
`managed_folder_id` 外键（1.41 / 2.09 ms）。这一格要追平就是 GAP 文档 §E 里"给 Trove 加一棵
链接文件夹树"那个 L+ 立项，不是加一个索引能补的。

## 这份报告不能说明什么

- **它不是端到端体验。** 全部查询数字都在数据层（Serpent 的 worker 进程、Trove 的
  `trove-core`），不含渲染、滚动帧率、图片解码上屏。Serpent 另有一套 e2e UI 基准
  （`tests/e2e/navigation-perf-benchmark.test.ts`、`large-library-scroll-benchmark.test.ts`，
  含事件循环延迟与首卡可见时间），本文没跑；Trove 侧也没有对应的帧计时设施。
- **镜像库不是 Trove 真实导入出来的库。** 镜像行没有 Trove 自己会挖的那些事实：调色板、
  主色、视觉签名、EXIF。真实库里 `extra` 更大（100k 镜像 387 B/行已经比 20k 镜像 76 B/行大），
  `json_extract` 类过滤会更贵一点，颜色相似度检索则完全没测。
- **fixture 是合成的。** 噪声图对解码器比真实照片更"均匀"，导入侧的绝对值不能外推到真实库；
  查询层不受影响（查询只看元数据行）。
- **Serpent 的 GUI 数字带 XWayland**（本机 Wayland 路径出不了窗）。
- **100k 的 Serpent 侧只有 2 轮重复**（20k 是 3 轮），且 100k 的 `needle` 档没测。
- **一次也没测写路径。** Serpent 的 `comprehensive-perf-bench` 里那组 trash / restore /
  永久删除 / 评分写入 / 合集重排指标默认关闭（`SERPENT_PERF_BENCH_ALLOW_MUTATION=1` 才开，
  且要求一次性库副本），Trove 侧对应指标没写。

## 附：仓库里现在有的东西

```text
bench/
  run-all.sh                    五个阶段的总入口
  process-bench.mjs             外部进程探测器（niri 出窗 + /proc 采样）
  aggregate.mjs                 配对表格生成器
  serpent/                      Serpent 侧的三个驱动 + 说明（README.md 记录两处环境绕行）
  results/                      serpent-lib-*.jsonl · trove-lib-*.jsonl · proc.jsonl · summary.md
crates/trove-core/examples/serpent_parity_bench.rs   Trove 的镜像基准（含 SQL 地板诊断）
```

进仓库的只有那 9 个源文件。`bench/work/`（29 GB + 143 GB 的 fixture）与 `bench/results/`
（原始读数 jsonl 与 `summary.md`）都是本地产物，`.gitignore` 已排除：前者由
`bench/serpent/gen-fixture.cjs` 重新生成（20k 46 s、100k 119 s），后者由 `bash bench/run-all.sh`
重跑。本文所有表格就是同一份 `aggregate.mjs` 画出来的，改一行读数就能重画。
