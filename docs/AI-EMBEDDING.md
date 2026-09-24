# AI 与嵌入 (AI & Embedding)

> 向量嵌入、语义搜索、模型管理 — 可扩展的 AI 能力层

---

## 概述

Trove 的 AI 层采用**提供者模式**（Provider Pattern），将"向量从哪来"和"向量怎么用"解耦：

- **Trove 拥有向量** — 存储、索引、过期判断全在本地
- **Provider 产出向量** — 任何兼容接口即可接入
- **未来可扩展** — 本地 CLIP、多模态端点只需实现同一 trait

```
┌─────────────────────────────────────────────────────┐
│                  EmbeddingProvider                   │
│                                                     │
│  ┌─────────────────┐     ┌─────────────────────┐   │
│  │ OpenAICompatible │     │    MockProvider     │   │
│  │ (远程 API)       │     │   (测试/开发)       │   │
│  └────────┬────────┘     └─────────────────────┘   │
│           │                                         │
│  ┌────────▼────────┐     ┌─────────────────────┐   │
│  │  LocalCLIP      │     │  multimodal         │   │
│  │  (规划中)       │     │  (规划中)           │   │
│  └─────────────────┘     └─────────────────────┘   │
└─────────────────────┬───────────────────────────────┘
                      │
                      ▼
              asset_embeddings 表
                      │
                      ▼
              VectorIndex (内存)
                      │
                      ▼
              余弦相似度检索
```

---

## EmbeddingProvider Trait

```rust
pub trait EmbeddingProvider: Send + Sync {
    fn id(&self) -> &str;           // 模型标识，如 "text-embedding-3-small"
    fn asset_space(&self) -> EmbeddingSpace;  // Text / Image
    fn dim(&self) -> Option<usize>; // 向量维度（可延迟学习）
    fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn embed_images(&self, paths: &[PathBuf]) -> Result<Vec<Vec<f32>>>; // 多模态 provider 才实现
}
```

### 关键契约

- **确定性身份**：不同 provider 必须产生不同 `id`，因为它是存储的 `model` 键
- **维度学习**：可从首次响应学习维度，无需预先配置
- **空间区分**：文本嵌入存 `Text` 空间；CLIP 图像嵌入存 `Image` 空间

---

## OpenAI 兼容 Provider

### 支持的端点

| 服务 | Base URL | 认证 |
|------|---------|------|
| OpenAI | `https://api.openai.com/v1` | API Key |
| Ollama | `http://127.0.0.1:11434/v1` | 无需 |
| LM Studio | `http://127.0.0.1:1234/v1` | 无需 |
| vLLM | 自定义 | 可选 |
| 代理 | 自定义 | 可选 |

### 请求参数

| 参数 | 值 | 说明 |
|------|-----|------|
| `REQUEST_BATCH` | 64 | 每请求输入数 |
| `REQUEST_TIMEOUT` | 60s | 单请求超时 |
| `RETRIES` | 2 | 重试次数 |
| `BACKOFF` | 1.5s × 2^n | 退避策略 |
| `MAX_BODY` | 64MB | 响应体上限 |

### 重试策略

- **429 / 5xx** → 退避重试
- **4xx (非 429)** → 立即失败（请求被拒绝）
- **传输错误** → 退避重试

---

## 嵌入文本构造

每个资产的嵌入输入按重要性排序拼接：

```
{title}                    // 或使用文件名（无标题时）
{description}              // 可选
{tag1}, {tag2}, {tag3}    // 标签列表
```

- 截断至 `MAX_TEXT_CHARS` (4000 字符)
- BLAKE3 哈希作为 `source_hash`
- 标题/描述/标签任一变更 → 标记过期 → 下次回填重新嵌入

---

## 向量存储

### SQLite 表结构

```sql
CREATE TABLE asset_embeddings (
    asset_id    UUID PRIMARY KEY,
    model       TEXT NOT NULL,        -- provider.id()
    space       TEXT NOT NULL,        -- "text" | "image"
    dim         INTEGER NOT NULL,
    vector      BLOB NOT NULL,        -- f32 数组
    source_hash TEXT NOT NULL,        -- 输入文本的 BLAKE3
    computed_at TIMESTAMP NOT NULL
);
```

### 过期判断

- `source_hash` 不匹配 → 过期
- 模型切换 → 全量过期

---

## 向量检索 (VectorIndex)

### 内存索引

- 库加载时从 `asset_embeddings` 表构建
- 纯 Rust 实现，无外部依赖
- 余弦相似度排序

### 查询流程

1. 用户输入查询文本
2. 调用 `provider.embed_texts([query])` 获取查询向量
3. 在 `VectorIndex` 中计算与所有资产向量的余弦相似度
4. 按相似度降序返回 Top-K

---

## 嵌入回填任务

### 任务参数

| 参数 | 值 | 说明 |
|------|-----|------|
| `BATCH` | 64 | 每批处理资产数 |
| `BUSY_TIMEOUT` | 5s | SQLite 忙等待超时 |

### 执行流程

```
1. 打开独立 SQLite 连接
2. 查询所有资产（含标签）
3. 跳过 source_hash 匹配的资产
4. 批量调用 provider.embed_texts()
5. 每批事务提交
6. 支持取消（检查 ctx.cancelled()）
```

### 进度报告

- `done` / `total` 进度
- `embedded` / `skipped` / `failed` 计数
- 取消或错误时返回 `EmbedOutcome`

---

## 图像嵌入（多模态）

### CLIP 风格联合空间

打开多模态模式后，**素材按缩略图建索引，查询仍是文本**——两个编码器分开，但被训练进同一个向量空间，所以向量可以直接比较。这就是「输入一个猫字，找到没有标签的猫照片」。

```
素材侧：embed_images(缩略图)  → Image 空间
查询侧：embed_texts("猫")     → 同一空间
                                 ↓
                          余弦相似度直接可比
```

开关：设置 ▸ AI 嵌入 ▸ **多模态（图像）嵌入**（`ai_embedding.multimodal`）。

### 回填任务的输入选择

回填按 provider 的 `asset_space()` 决定每个素材嵌什么：

| provider 空间 | 素材有缩略图 | 嵌什么 |
|--------------|------------|--------|
| `Text` | — | 元数据文本（标题 / 描述 / 标签） |
| `Image` | ✅ | **缩略图**，指纹用 `content_hash` |
| `Image` | ❌（字体、音频、无预览格式） | 回退到元数据文本——CLIP 的文本编码器落在同一空间，所以素材仍然可搜 |

一批里可以同时有图像和文本输入，任务会把它们拆成两次调用再按顺序拼回去。

### 端点要求

多模态模式需要**联合嵌入端点**。OpenAI 的 `/embeddings` **不支持图像输入**；已实现的是 OpenAI 兼容的「对象输入」形状：

```json
{"model": "jina-clip-v2", "input": [{"text": "猫"}]}
{"model": "jina-clip-v2", "input": [{"image": "data:image/jpeg;base64,..."}]}
```

> 其他形状（火山方舟 `doubao-embedding-vision` 的 `/embeddings/multimodal`、DashScope 的 `input.contents`、Jina 自托管的 `image_base64`）目前没有适配，需要另写 provider。

### 与多模态分析的区别

| | 图像嵌入（本节） | [多模态分析](./AI-TAGGING.md) |
|---|---|---|
| 产出 | 一个向量，不产生标签 | 人类可读的标签 / 描述 / 评分 |
| 成本 | 便宜 | 贵（每个素材一次 VLM 调用） |
| 可编辑 / 可复用 | ❌ | ✅ |
| 走全文索引 / 智能集合 | ❌ | ✅ |

两者互补：嵌入负责「文搜图」的召回，分析负责产出可复用的标签资产。只上嵌入而元数据为空时，向量腿实际上只按文件名相似度召回，反而会给 RRF 融合引入噪声。

---

## 本地推理（规划中）

| 模型 | 大小 | 用途 |
|------|------|------|
| CLIP ViT-B/32 | ~330MB | 本地文本-图像联合嵌入 |
| MobileNetV3 | ~15MB | 本地图像标签 |
| RMBG-1.4 | ~80MB | 本地智能抠图 |

---

## 配置

### 应用配置 (`config.json`)

```json
{
  "ai_embedding": {
    "base_url": "https://api.openai.com/v1",
    "api_key": "sk-...",
    "model": "text-embedding-3-small",
    "multimodal": false
  }
}
```

`multimodal: true` 时端点必须是联合嵌入模型（如 `jina-clip-v2`），素材改按缩略图嵌入。

### 设置页

- Base URL 输入框
- API Key 输入框
- 模型名称输入框
- **多模态（图像）嵌入**开关
- 测试连接按钮
- 手动触发回填按钮

---

## 代码位置

| 文件 | 内容 |
|------|------|
| `trove-core/src/ai/mod.rs` | EmbeddingProvider trait、文本构造 |
| `trove-core/src/ai/openai.rs` | OpenAI 兼容客户端 |
| `trove-core/src/ai/mock.rs` | 测试用 Mock 提供者 |
| `trove-core/src/search/vector.rs` | 内存向量索引 |
| `trove-core/src/store/embeddings.rs` | 嵌入存储 CRUD |
| `trove-core/src/tasks/embed.rs` | 嵌入回填任务 |
| `trove-app/src/dialogs/settings/ai.rs` | AI 设置页 |
