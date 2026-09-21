# 搜索与索引 (Search & Indexing)

> 全文搜索、视觉搜索、向量语义搜索、智能集合 — 多维度素材检索

---

## 概述

Trove 提供四层检索能力，从精确匹配到语义理解：

| 层级 | 引擎 | 能力 |
|------|------|------|
| 全文搜索 | Tantivy | 文件名/标题/描述/标签，支持中文分词、拼音、子串 |
| 视觉搜索 | pHash + 直方图 | 以图搜图、颜色相似度 |
| 向量搜索 | 内存索引 | 语义搜索（需配置嵌入模型） |
| 智能集合 | JSON 查询树 | 规则驱动的虚拟文件夹 |

---

## 全文搜索 (Tantivy)

### 架构

```
SQLite (source of truth)
       │
       │ search_queue 表 (触发器自动入队)
       ▼
  Tantivy Index Writer
       │
       ▼
  search_index/ 目录
       │
       ▼
  Tantivy Index Reader ← 查询
```

- **SQLite 是唯一真相源** — 索引是可丢弃的派生产物
- **异步更新** — 通过 `search_queue` 出盒表 + 触发器自动入队
- **版本化** — 索引版本不匹配时自动重建

### 索引字段

| 字段 | 分词方式 | 说明 |
|------|---------|------|
| `file_name` | jieba + n-gram + pinyin | 文件名 |
| `title` | jieba + n-gram + pinyin | 标题 |
| `description` | jieba + n-gram + pinyin | 描述 |
| `tags` | jieba + n-gram + pinyin | 标签名 |

### 查询能力

| 匹配类型 | 示例 | 说明 |
|---------|------|------|
| 词语匹配 | `sunset` → "Sunset" | jieba 分词 + 模糊容错 |
| 子串匹配 | `sunse` → "Sunset" | 2-3 gram 索引 |
| 拼音匹配 | `mao` → "猫" | 全拼 + 首字母 |
| 中文分词 | `花园里的猫` → 匹配"花园"、"猫" | jieba 中文分词 |

### 筛选条件

搜索可叠加任意筛选：
- 资产类型（图片/视频/音频/字体/3D/文档）
- 评分（1-5 星）
- 收藏状态
- 标签
- 颜色
- 拍摄日期
- 宽高比
- 方向（横/竖/方）
- 形状比例预设（2.35:1、16:9、9:16、4:3、1:1 等）

---

## 视觉搜索

### 感知哈希 (pHash)

- 9×8 差异哈希，对缩放/压缩/轻微编辑鲁棒
- 汉明距离 ≤ 10 视为相似
- 导入时自动计算，零推理依赖

### 颜色直方图

- RGB 三维直方图
- 支持按颜色相似度筛选
- 配合主色板使用

### 以图搜图

1. 选择参考图片
2. 计算 pHash + 颜色直方图
3. 在索引中检索最相似的 Top-K 结果
4. 按综合相似度排序

---

## 向量语义搜索

### 架构

```
┌─────────────────────────────────────────┐
│           EmbeddingProvider              │
│  ┌──────────────┐  ┌──────────────────┐ │
│  │ OpenAI 兼容   │  │ MockProvider     │ │
│  │ (远程 API)   │  │ (测试用)         │ │
│  └──────────────┘  └──────────────────┘ │
└─────────────────┬───────────────────────┘
                  │ embed_texts()
                  ▼
         asset_embeddings 表
                  │
                  ▼
         VectorIndex (内存)
                  │
                  ▼
         余弦相似度检索
```

### 嵌入模型

- 支持任何 OpenAI 兼容 API（OpenAI / Ollama / LM Studio / vLLM）
- 文本嵌入：`text-embedding-3-small` 等
- 图像嵌入：CLIP 风格多模态模型（需实现 `embed_images`）

### 嵌入文本构造

每个资产的嵌入输入 = 标题 + 描述 + 标签，按重要性排序：

```
{title}
{description}
{tag1}, {tag2}, {tag3}
```

- 截断至 4000 字符
- SHA-256 哈希作为 `source_hash`，内容变更时自动标记过期

### 回填任务

- 后台批量嵌入
- 跳过未变更的资产（source_hash 匹配）
- 支持取消和进度报告

---

## 智能集合 (Smart Collections)

### 概念

规则驱动的虚拟文件夹 — 不移动文件，按条件动态归集。

### 规则类型

| 规则 | 说明 |
|------|------|
| `rating` | 评分比较（≥, =, ≤） |
| `kind` | 资产类型 |
| `text` | 文件名/标题/描述包含 |
| `tag` | 包含标签（含子树） |
| `favorite` | 收藏状态 |
| `color` | 主色相似 |
| `date` | 拍摄日期范围 |
| `aspect_ratio` | 宽高比 |
| `orientation` | 方向（横/竖/方） |

### 组合逻辑

```json
{
  "operator": "and",
  "conditions": [
    { "field": "rating", "op": ">=", "value": 3 },
    { "field": "kind", "op": "is", "value": "image" },
    { "operator": "or", "conditions": [
      { "field": "tag", "op": "contains", "value": "风景" },
      { "field": "tag", "op": "contains", "value": "人像" }
    ]}
  ]
}
```

- 支持 `and` / `or` 嵌套组合
- 编译期校验（无效规则拒绝保存）
- 自动包含子标签

---

## 搜索队列机制

SQLite 触发器自动维护 `search_queue` 表：

```sql
-- asset 写入 → 入队
CREATE TRIGGER asset_after_insert AFTER INSERT ON assets
BEGIN
    INSERT INTO search_queue (asset_id, operation) VALUES (NEW.id, 'upsert');
END;

-- tag 变更 → 入队
CREATE TRIGGER tag_after_change AFTER INSERT OR DELETE ON asset_tag
BEGIN
    INSERT INTO search_queue (asset_id, operation) VALUES (NEW.asset_id, 'upsert');
END;
```

- UI 线程定期 `drain()` 队列
- 批量更新 Tantivy 索引
- 索引损坏时重建：全量入队 + 重新 drain

---

## 代码位置

| 文件 | 内容 |
|------|------|
| `trove-core/src/search.rs` | Tantivy 全文搜索引擎 |
| `trove-core/src/search/vector.rs` | 向量索引与语义搜索 |
| `trove-core/src/ai/mod.rs` | EmbeddingProvider trait |
| `trove-core/src/ai/openai.rs` | OpenAI 兼容嵌入客户端 |
| `trove-core/src/store/visual_search.rs` | 视觉搜索存储 |
| `trove-core/src/store/smart_collections.rs` | 智能集合持久化 |
| `trove-core/src/tasks/embed.rs` | 嵌入回填任务 |
