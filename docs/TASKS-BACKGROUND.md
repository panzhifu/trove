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
- **单例约束** — 同类型任务同时只能运行一个
- **协作取消** — 任务在检查点检查 `cancelled` 标志
- **进度节流** — 进度事件间隔 ≥ 100ms，避免 UI 过载
- **两把锁** — 任务表和事件队列各持一把 `Mutex`，互不阻塞：上报进度只碰事件锁，`poll_events()` 不再排在上报之后。两者都需获取时顺序恒为 `jobs → events`

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
   │            │
   │            ├──► Cancelled (用户取消)
   │            │
   │            └──► Failed (错误)
   │
   └──► AlreadyRunning
```

### 任务状态

```rust
pub enum TaskStatus {
    Running,     // 运行中
    Completed,   // 已完成
    Failed,      // 失败
    Cancelled,   // 已取消
}
```

---

## 任务事件

UI 通过 `poll_events()` 获取任务状态更新（只取事件锁，因此不会被正在上报进度的任务挡住）：

```rust
pub enum TaskEvent {
    Started { id, kind },
    Progress { id, done, total },
    Completed { id, kind, summary },
    Failed { id, kind, error },
    Cancelled { id, kind },
}
```

### 事件消费模式

```rust
// UI 线程定期调用（如每 100ms）
let events = task_manager.poll_events();
for event in events {
    match event {
        TaskEvent::Progress { id, done, total } => {
            update_progress_bar(id, done, total);
        }
        TaskEvent::Completed { id, kind, summary } => {
            show_notification(format!("{}: {}", kind, summary));
        }
        // ...
    }
}
```

---

## JobContext — 任务执行接口

每个任务运行时获得一个 `JobContext`，用于报告进度和检查取消：

```rust
impl JobContext {
    pub fn cancelled(&self) -> bool;     // 检查是否请求取消
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

### 任务进度条

- 底部状态栏显示当前任务进度
- 点击展开任务列表
- 每个任务可单独取消

### 任务列表

```
┌─────────────────────────────────────┐
│ 📥 导入中...  123/456  [取消]       │
│ 🔄 AI 嵌入...  78/200   [取消]      │
└─────────────────────────────────────┘
```

### 通知

- 任务完成 → 摘要通知
- 任务失败 → 错误通知（可重试）
- 任务取消 → 静音（用户主动操作）

---

## 代码位置

| 文件 | 内容 |
|------|------|
| `trove-core/src/tasks/mod.rs` | TaskManager、JobContext、事件类型 |
| `trove-core/src/tasks/import.rs` | 导入任务 |
| `trove-core/src/tasks/watch.rs` | 监视扫描任务 |
| `trove-core/src/tasks/embed.rs` | 嵌入回填任务 |
| `trove-app/src/library/jobs.rs` | UI 层任务协调 |
