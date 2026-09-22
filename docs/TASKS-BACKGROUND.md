# 任务与后台作业 (Tasks & Background Jobs)

> 任务管理器、进度报告、取消机制、失败重试 — 后台作业基础设施

---

## 概述

Trove 的后台任务系统管理所有长时间运行的操作，包括：

| 任务类型 | 说明 |
|---------|------|
| 导入 | 批量文件导入 |
| 收件箱收集 | 扫描监视文件夹 |
| 模型预览 | 3D 模型缩略图生成 |
| 视频解码 | 视频海报帧提取 |
| 批量转换 | 图片格式转换 |
| 维护 | 完整性校验、清理 |
| 视觉回填 | pHash/直方图重算 |
| 监视扫描 | 文件系统监听事件处理 |
| 嵌入回填 | AI 向量嵌入计算 |

---

## 任务管理器架构

```
┌─────────────────────────────────────────────────────┐
│                    TaskManager                       │
│                                                     │
│  ┌───────────────────────┐┌───────────────────────┐│
│  │ jobs                  ││ events                ││
│  │ Mutex<HashMap<TaskId, ││ Mutex<VecDeque<Task-  ││
│  │       JobState>>      ││        Event>>        ││
│  └───────────────────────┘└───────────────────────┘│
│                                                     │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────┐  │
│  │ Import Job   │  │ Embed Job    │  │ Watch Job│  │
│  └──────────────┘  └──────────────┘  └──────────┘  │
└─────────────────────────────────────────────────────┘
```

### 设计原则

- **UI 无关** — 任务在 `std::thread` 上运行，无 UI 类型
- **单例约束** — 同类型任务同时只能运行一个（`Running` **或** `Paused` 都占位，用 `is_active` 判定）
- **协作取消与暂停** — 任务在检查点检查 `cancelled` 标志、并在 `park_if_paused` 处挂起，两者都只在已提交批次之间生效
- **进度节流** — 进度事件间隔 ≥ 100ms，避免 UI 过载
- **两把锁** — 任务表和事件队列各持一把 `Mutex`，互不阻塞：上报进度只碰事件锁，`poll_events()` 不再排在上报之后。两者都需获取时顺序恒为 `jobs → events`
- **按作业分桶** — 事件按 `task_id` 各存一桶：`poll_events_for(id)` 只搬走自己那一桶，代价是自己桶的长度而不是全队列的长度；并发作业的 watcher 既吞不到对方的事件，也挤不掉对方的终态事件
- **可以等待** — `wait_events_for(id, timeout)` 睡在桶的 `Condvar` 上，作业一上报就醒，省掉轮询节拍；它挂住调用线程，所以只给自持线程的嵌入方（CLI、测试）用

---

## 任务类型

```rust
pub enum TaskKind {
    Import,              // 文件导入
    CollectInbox,        // 收件箱收集
    ModelPreview,        // 3D 模型预览
    VideoDecode,         // 视频解码
    BatchConvert,        // 批量转换
    Maintenance,         // 维护任务
    VisualBackfill,      // 视觉签名回填
    WatchScan,           // 监视扫描
    EmbeddingBackfill,   // 嵌入向量回填
    AutoTag,             // AI 自动打标签（一个素材一次请求，最慢的作业）
}
```

### 互斥约束

| 任务类型 | 并发限制 |
|---------|---------|
| Import | 1 |
| CollectInbox | 1 |
| ModelPreview | 1 |
| VideoDecode | 1 |
| BatchConvert | 1 |
| Maintenance | 1 |
| VisualBackfill | 1 |
| WatchScan | 1 |
| EmbeddingBackfill | 1 |

同类型任务第二次 `start()` 返回 `StartError::AlreadyRunning`。

---

## 任务生命周期

```
start() ──► Running ──► Completed
   │            │  ▲
   │            │  └── resume() ◄── pause()
   │            ▼
   │          Paused ──► Cancelled (暂停中被取消)
   │            │
   ├──► Cancelled (用户取消)
   │
   └──► AlreadyRunning

Running / Paused ──► Failed (错误)
Failed / Cancelled ──► (面板“重试”按钮以原输入重新 start())
```

### 任务状态

```rust
pub enum TaskStatus {
    Running,     // 运行中
    Paused,      // 已暂停：仍占用同类型槽位，等待 resume 或被取消
    Completed,   // 已完成
    Failed,      // 失败（可在面板重试）
    Cancelled,   // 已取消（可在面板重试）
}
```

暂停是协作式的：`pause()` 只置位，作业在**下一个检查点**（已提交批次之间，绝不跨事务）
用 `JobContext::park_if_paused()` 挂起在 `Condvar` 上；`resume()` 唤醒它继续，`cancel()`
也会唤醒它以便在暂停状态下仍能尽快收尾。

---

## 任务事件

UI 通过事件队列获取任务状态更新（只取事件锁，因此不会被正在上报进度的任务挡住）。
队列是**按作业分桶**的一张 `HashMap`：每个 `task_id` 一桶，watcher 各取各的。
一个只跟踪单个作业的 watcher 用 `poll_events_for(id)`——它把自己那一桶整个取走，
别的桶原样留给各自的 watcher。分桶之前是一条全体共用的 FIFO，它有两个毛病：挑出自
己的事件要扫过整条队列（还要重建别人的），而全局容量上限让无人轮询的常驻作业能把
别人正在等的 `Completed` 挤掉——正是过去“进度数字不更新”的根因：

```rust
pub enum TaskEvent {
    Started { id, kind },
    Progress { id, done, total },
    Completed { id, kind, summary },
    Failed { id, kind, error },
    Cancelled { id, kind },
    Paused { id, kind },    // 进入 Paused
    Resumed { id, kind },   // 离开 Paused
}

impl TaskEvent {
    pub fn task_id(&self) -> TaskId;   // 每个变体都带 id，分桶就按它路由
}
```

### 事件消费模式

```rust
// 定期调用（如每 80ms），只取本作业的事件
let events = task_manager.poll_events_for(task_id);
for event in events {
    match event {
        TaskEvent::Progress { done, total, .. } => {
            // done 与 total 都要落库：total 常在中途才由 set_total 知晓
            update_progress_bar(id, done, total);
        }
        TaskEvent::Completed { id, kind, summary } => {
            show_notification(format!("{}: {}", kind, summary));
        }
        // ...
    }
}

// 或者：自持一个线程的嵌入方睡到本作业有事件为止，超时兜底
let events = task_manager.wait_events_for(task_id, Duration::from_millis(500));
```

`wait_events_for` 挂住调用线程，所以它不能用在共享线程池上——gpui 的 background
池是固定数目的 worker，一个睡死的 watcher 就少一个能跑别的后台任务的 worker，
所以 UI 侧的 `watch_job` 仍然按 `POLL_INTERVAL` 轮询（现在每轮只是一次哈希查找加
搬走自己的桶）。整条 watcher 循环在 `trove-app/src/library/jobs/mod.rs`，三种作
业（导入、向量回填、AI 标注）共用它，各自只提供自己的 toast key 与文案。

事件桶有两条上限：单桶 `MAX_EVENTS_PER_JOB` 条，满了只丢自己最旧的事件（常驻作业
淘汰的是自己的陈旧进度，碰不到别人的终态）；桶数 `MAX_TRACKED_JOBS` 个，超了整桶
丢掉创建最早的——被丢的必然是长期没人读的作业，正在被 watch 的作业每轮都被取空，
排不到它。

---

## JobContext — 任务执行接口

每个任务运行时获得一个 `JobContext`，用于报告进度和检查取消：

```rust
impl JobContext {
    pub fn cancelled(&self) -> bool;     // 检查是否请求取消
    pub fn park_if_paused(&self);        // 若已暂停则挂起到 resume/取消
    pub fn cancel_flag(&self) -> &AtomicBool; // 供并行循环内部检查
    pub fn set_total(&self, total: u64); // 设置总工作量
    pub fn progress(&self, done: u64, total: u64); // 报告进度
    pub fn set_summary(&self, summary: String);    // 设置完成摘要
}
```

### 典型任务实现

```rust
fn run_import(opts: ImportOptions, ctx: &JobContext) -> Result<ImportOutcome, String> {
    let files = collect_files(&opts.source);
    ctx.set_total(files.len() as u64);
    
    let mut outcome = ImportOutcome::default();
    for (i, file) in files.iter().enumerate() {
        if ctx.cancelled() {
            outcome.cancelled = true;
            return Ok(outcome);
        }
        match import_one(file) {
            Ok(item) => outcome.imported.push(item),
            Err(e) => outcome.skipped.push(ImportSkip {
                path: file.clone(),
                reason: e,
            }),
        }
        ctx.progress(i as u64 + 1, files.len() as u64);
    }
    Ok(outcome)
}
```

---

## 取消机制

### 协作式取消

- 不强制终止线程
- 任务在检查点检查 `ctx.cancelled()`
- 清理资源后优雅退出

### 取消流程

```
用户点击取消
    │
    ▼
task_manager.cancel(task_id)
    │
    ▼
设置 cancel 标志 (AtomicBool)
    │
    ▼
任务在下一次检查点检测到 → 退出循环
    │
    ▼
发送 TaskEvent::Cancelled
```

---

## 进度报告

### 进度节流

- 默认节流间隔：100ms
- 最终单位（done == total）总是立即报告
- 避免高频更新拖慢 UI

### 进度计算

```rust
// 已知总量
ctx.set_total(1000);
for i in 0..1000 {
    process_item(i);
    ctx.progress(i + 1, 1000);  // 内部节流
}

// 未知总量（流式处理）
ctx.set_total(0);  // 未知
loop {
    let batch = next_batch();
    if batch.is_empty() { break; }
    ctx.progress(processed, processed + batch.len());  // 增量
}
```

---

## 监视文件夹

### 架构

```
┌─────────────────────────────────────────┐
│          文件系统监听 (notify crate)      │
│  - inotify (Linux)                      │
│  - FSEvents (macOS)                     │
│  - ReadDirectoryChangesW (Windows)      │
└─────────────────┬───────────────────────┘
                    │ 事件流
                    ▼
            ┌──────────────┐
            │  防抖缓冲     │  ← 合并短时间内的多次事件
            └──────┬───────┘
                   │
                   ▼
            ┌──────────────┐
            │  扫描任务     │  ← WatchScan
            │  - 新增文件   │
            │  - 修改文件   │
            │  - 删除文件   │
            └──────────────┘
```

### 监视策略

| 策略 | 说明 |
|------|------|
| 实时监听 | 内核事件通知（首选） |
| 定期扫描 | 兜底（网络挂载、FUSE 无事件） |
| 防抖 | 合并短时间内的多次事件 |
| 忽略规则 | Git 风格：读取被扫描文件夹自己的 `.gitignore` / `.ignore` / `.git/info/exclude`，见 `tasks/ignore.rs` |

### 忽略规则

被监视（以及被拖拽导入）的文件夹用 Git 的文件来说明「这些不要」，两条扫描路径读同一套规则：

- 优先级同 Git：规则相对于声明它的文件夹；最深的一份文件对某路径有最终决定权；同一文件内最后匹配的一行胜出；结尾 `/` 只匹配目录。
- 被忽略的目录不进入，其下内容也就无法用 `!` 反选捞回 —— 与 `git check-ignore` 的行为一致。
- 扫描时忽略文件来自已经读到的目录列表，不额外增加 `open()`；事件路径按目录缓存读取结果，并随设置的重读节奏失效，改完 `.gitignore` 不用重启。
- 静默跳过，不进导入报告的 skips：这是用户自己写的策略，不是「无法表示」。

---

## 错误处理

### 任务级错误

- 任务返回 `Err(String)` → 标记为 `Failed`
- 错误信息在 `TaskEvent::Failed` 中传递
- UI 显示错误通知

### Panic 处理

- 使用 `catch_unwind` 捕获 panic
- Panic 转为 `Failed` 状态
- 包含 panic 信息

```rust
let outcome = panic::catch_unwind(AssertUnwindSafe(|| run(&ctx)));
match outcome {
    Ok(Ok(value)) => { /* 成功 */ }
    Ok(Err(error)) => { /* 业务错误 */ }
    Err(_) => { /* panic */ }
}
```

---

## UI 集成

### 状态栏任务中心

底部状态栏有一段紧凑摘要（最新一个进行中/暂停作业的名字与**实时** `done/total`），
点击向上弹出一个 `Popover` 任务面板，列出所有进行中与最近结束的作业。
App 侧的状态镜像在 `LibraryController::tasks`（`Vec<TaskCard>`）：各 watcher 在作业
启动时 `begin_task`、在事件里更新进度/状态，因此面板数字与通知一致；已结束的卡片会保留
一小段（`prune_tasks` 限长）以便重试，核心注册表会在下一个作业启动时清理它们。

### 任务面板

```
┌────────────────────────────────────────────┐
│ 后台任务                       [清除已完成] │
│ 导入      运行中   123/456   [暂停][取消]  │
│ AI 嵌入   已暂停   78/200    [继续][取消]  │
│ AI 分析   失败     56/300    [重试]        │
└────────────────────────────────────────────┘
```

每个作业按其状态给出按钮：运行中 = 暂停 / 取消；已暂停 = 继续 / 取消；失败或已取消 =
重试（用启动时记录的 `Retryable` 输入重新发起：导入用 `ImportOptions`，嵌入/分析从当前
配置重建 provider + `AiAnalysisRunRequest`）。

### 通知

- 任务完成 → 摘要通知
- 任务失败 → 错误通知
- 进度通知的数字现在正确更新：每个 watcher（三种作业共用 `library/jobs` 的 `watch_job`）
  用 `poll_events_for(task_id)`
  只取自己作业的事件（不再互相吞事件），导入进度也同时携带 `total`（不再卡在“正在扫描”）
- 任务取消 → 静音（用户主动操作）

---

## 代码位置

| 文件 | 内容 |
|------|------|
| `trove-core/src/tasks/mod.rs` | TaskManager、JobContext、事件类型 |
| `trove-core/src/tasks/import.rs` | 导入任务 |
| `trove-core/src/tasks/watch.rs` | 监视扫描任务 |
| `trove-core/src/tasks/embed.rs` | 嵌入回填任务 |
| `trove-app/src/library/jobs/` | UI 层任务协调（`mod.rs` 的 `watch_job` 是所有作业共用的轮询循环） |
