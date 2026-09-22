# AI 自动打标签 (AI Tagging)

> 让聊天模型读素材、给素材起标签。与 [AI-EMBEDDING.md](./AI-EMBEDDING.md) 互补:
> 那边把素材变成向量用于**检索**,这边让模型**说出它看到了什么**。

---

## 概述

自动打标签做的事只有一件:**把图片(或素材的事实)交给一个多模态聊天模型,把它回答的标签写进库。**

四条设计约束,全部来自「标签是用户可见且难以收回的资产」:

| 约束 | 做法 |
|------|------|
| 不能让标签体系爆炸 | 把库里**已有标签**喂给模型优先复用,新词有配额,且统一挂在父标签下 |
| 不能在用户没要求时花钱 | 只由显式命令/按钮触发;**导入不自动打标签** |
| 重复运行必须免费 | 每个素材记指纹(模型 + prompt 版本 + 内容哈希 + 事实),命中即跳过 |
| 必须能整体撤回 | 每次运行写了什么记在素材的 `facts` 里,`--undo` 按记录摘除 |

打标签**没有**图像嵌入,不需要 CLIP/ONNX;它要的是一个 `chat/completions` 端点。

---

## ChatProvider Trait

`crates/trove-core/src/ai/chat.rs`

```rust
pub trait ChatProvider: Send + Sync {
    /// 模型标识,随每个模型产出的标签一起存,便于日后辨认是谁干的活。
    fn id(&self) -> &str;

    /// 回答一次提问。回复是自由文本,解析归调用方(见 tagging)。
    fn complete(&self, request: &ChatRequest<'_>) -> Result<String>;
}

pub struct ChatRequest<'a> {
    pub system: &'a str,          // 每次运行固定:角色 + 规则 + 词表
    pub user: &'a str,            // 本次素材:事实摘要 + 是否有图
    pub image: Option<&'a [u8]>,  // 缩略图 JPEG 字节;None = 纯文本请求
}
```

同步 trait,理由与 `EmbeddingProvider` 相同:它跑在任务的 worker 线程上,再引一个 async runtime 只会多一个 runtime。

### OpenAI 兼容实现

`OpenAIChat` 打 `POST {base_url}/chat/completions`:

```json
{
  "model": "gpt-4o-mini",
  "messages": [
    {"role": "system", "content": "…规则与词表…"},
    {"role": "user", "content": [
      {"type": "text", "text": "file name: …\ndimensions: …"},
      {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,…"}}
    ]}
  ],
  "temperature": 0.2,
  "max_tokens": 512
}
```

- 缩略图走 `data:` URI(需要 `base64` 依赖)。**用缩略图而不是原图**:`media/thumb.rs` 的 512px JPEG 对识别足够,且省带宽与 token。
- 超时 120 s、重试 2 次(429/5xx/传输错误才重试,4xx 直接失败)、错误解包与 embedding 侧共用 `ai/http.rs`。
- 纯文本请求时 `content` 是字符串,带图时是数组 —— 两种形状服务端都认。

---

## 提示词构造

`crates/trove-core/src/ai/tagging.rs`

### system prompt(每次运行一次)

写英文 —— 指令遵循是在英文上调优的;**只有标签本身**按目标语言要求。内容:回复必须是 JSON 数组、一个概念一个标签、最多三个词、最多 N 个标签、空数组也是合法答案、**优先复用词表**、新词配额、以及「用 X 语言写标签」。

### asset digest(每个素材一份)

模型看的是 `asset_digest()`:

```
file name: IMG_4821.jpg
title: 海边日落
kind: image
dimensions: 4000x3000 (landscape)
captured: 2026-08-14
camera: Canon EOS R6
exposure: 35mm, f/1.8, 1/500s, ISO 400
location: 39.9042, 116.4074
dominant colour: #3b6ea5
already tagged: 旅行
```

相机、焦段、GPS、主色这些**图片本身看不到、文件名也说不出来**的事实,是纯文本模型唯一的抓手;有图时它们是补充。

### 图片说明写在 user 消息里

`tagging::image_note(attached)` 而不是 system prompt:有没有缩略图是**单个素材**的属性(视频没解出帧、格式没有预览),system 里写死「图已附上」对它们就是错的。

---

## 标签解析与归一化

模型说什么都可能发生,所以回复先尝试解析 JSON 数组,失败则回退按行/逗号切分;每个候选都要过 `normalize_tag()`:

- 剥列表标记(`1. ` / `- ` / `• ` / `* `)—— 但 `24mm` 这类数字开头的标签要留住
- 剥两端引号、括号、围栏、尾随标点
- 折叠内部空白、拒绝控制字符(检查在折叠**之前**,否则换行会被洗成空格)
- 上限 40 字符 / 最多 3 个词 —— 「a photograph of a cat sitting on a wooden table」是一句话,不是标签
- 按大小写不敏感去重(标签库本就 NOCASE:`Cat` 与 `cat` 是同一个)
- 整个素材最多 12 个

---

## 任务执行

`crates/trove-core/src/tasks/autotag.rs`,骨架照 `tasks/embed.rs`,三处不同:

| 点 | embedding 回填 | 自动打标签 |
|---|---|---|
| 请求单位 | 一次 64 条 | **一次一个素材**(聊天模型一次一段对话) |
| 并发 | 批内串行 | **小线程池并发请求**(默认 4,`--threads` / `TROVE_AUTOTAG_THREADS`) |
| 跳过依据 | `asset_embeddings.source_hash` | `facts["ai_tags"].digest`(不动 schema) |

流程:开私有 SQLite 连接 → 读词表(用得多的在前)→ 扫候选(活的、按导入时间倒序、可 `--ids`/`--limit` 收窄)→ 逐个算摘要与指纹、命中即 `skipped` → 分批并发提问 → **串行写库**(标签 + 指纹)→ 报进度。

### 图片被拒时自动降级

文本模型会拒收带图请求。任务对此的处理是:**发现一次,本次运行后续全部纯文本**:

```rust
Err(Error::Validation(_)) => {
    // 端点拒绝了请求本身,而图片是纯文本服务唯一会拒绝的部分
    rejected.store(true);
    // 落到纯文本重试
}
Err(other) => return Err(other),  // 传输错误已经重试过,不该被静默降级掩盖
```

只对 4xx 降级 —— 那才是「服务端在回答」;网络故障降级会把坏端点伪装成质量下降。结果的 `images_rejected` 会如实报出来。

### 写标签

- 已有同名标签 → 复用(不新建)
- 没有 → 新建,并挂到父标签(`ChatConfig::new_tag_parent`,默认 `AI`)下
- 父标签**按需创建**:一次什么都没建议的运行、或 `--dry-run`,不会留下一个空的 `AI` 标签
- 本次运行新建的标签立刻进词表,运行到第 20 个素材时不会把第 1 个刚造的词再发明一遍

### 指纹与幂等

```
fingerprint = BLAKE3(model ‖ prompt_version ‖ max_new_tags ‖ language ‖ content_hash ‖ digest(不含已有标签))
```

**已有标签刻意不计入**:本运行正要改它,把它当输入会让每个打完标签的素材永久失效、第二次运行把整库重问一遍(实现时踩到过)。

指纹、模型、时间、本次新增的标签一起写进 `facts["ai_tags"]`,**合并而非覆盖** —— `--force` 重跑时新增为空,覆盖会悄悄让上一次的运行变得不可撤销(也踩到过)。

---

## 撤销

`trove autotag --undo`(或 `Library::start_auto_tag_undo`)。

库自带的 undo 栈是**内存里的**、属于填它的那个进程,后台任务指望不上;所以靠素材上的记录。撤销的动作:按记录摘除标签 → 清掉 `facts["ai_tags"]`(于是下次运行会重新打)→ 报告**被这次撤销清空的**标签。

空标签只报告不删除:其中一个可能是用户手工建的,存储层分辨不了。

---

## 配置

`config.json`:

```json
{
  "ai_chat": {
    "base_url": "https://api.openai.com/v1",
    "api_key": "",
    "model": "gpt-4o-mini",
    "send_images": true,
    "max_new_tags": 3,
    "new_tag_parent": "AI",
    "tag_language": null
  }
}
```

与 `ai_embedding` **分开配置**:两者常常是同一台服务器上的不同模型,只配其中一个才是常态。`is_configured()` 判定「模型名 + base_url 非空」。

---

## 命令行

```sh
trove autotag --dry-run              # 只数有多少要处理,不发请求、不写库
trove autotag                        # 按库里的配置跑
trove autotag --limit 50             # 先试 50 个
trove autotag --no-images            # 纯文本端点,省掉那次「被拒」
trove autotag --max-new-tags 0       # 只复用现有标签,一个都不新建
trove autotag --parent-tag 参考      # 新词挂到别的父标签下
trove autotag --language zh-CN       # 标签语言
trove autotag --force --limit 1      # 忽略指纹重打一个
trove autotag --undo                 # 把历次自动打的标签全部摘掉
```

输出的 JSON 含 `planned` / `tagged` / `unchanged` / `skipped` / `failed` / `created_tags` / `images_rejected`。

---

## 代码位置

| 文件 | 内容 |
|------|------|
| `crates/trove-core/src/ai/chat.rs` | `ChatProvider` trait、`OpenAIChat`、响应与 data URI 解析 |
| `crates/trove-core/src/ai/tagging.rs` | 资产摘要、system prompt、标签解析与归一化 |
| `crates/trove-core/src/ai/http.rs` | 两个 provider 共用的 HTTP 小工具(读体、错误解包、截断) |
| `crates/trove-core/src/tasks/autotag.rs` | 引擎:候选、指纹、并发提问、写标签、撤销 |
| `crates/trove-core/src/config.rs` | `ChatConfig` |
| `crates/trove-core/src/library.rs` | `start_auto_tag` / `start_auto_tag_undo` / `auto_tag_options` |
| `crates/trove-cli/src/write.rs` | `trove autotag` |
| `crates/trove-core/src/crate::tasks` | `TaskKind::AutoTag`(与其它任务一样互斥、可取消、有进度) |

---

## 尚未做

- **界面入口**:设置页的配置块、选中素材触发、进度 toast —— 目前只有 CLI。
- 音频/视频的**内容**理解(目前只有元数据文本,视频封面帧可作图片但未接)。
- 标签**层级建议**:模型可以复用层级标签,但不会被要求提出新层级。
- 按已打标签的结果做**二次确认**(人工 review 队列)。
