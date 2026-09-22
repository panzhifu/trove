# 标签与集合 (Tags & Collections)

> 层级标签体系、合集组织、智能合集、批量操作 — 灵活的素材组织方式

---

## 概述

Trove 提供三层组织体系：

| 层级 | 说明 | 物理/虚拟 |
|------|------|----------|
| 标签 (Tags) | 层级嵌套、色标、计数 | 虚拟 |
| 合集 (Collections) | 嵌套文件夹、多对多 | 虚拟 |
| 智能合集 (Smart Collections) | 规则驱动、动态归集 | 虚拟 |

所有组织均为**虚拟** — 不移动原文件，不影响物理存储。

---

## 标签系统

### 数据模型

```sql
CREATE TABLE tags (
    id          UUID PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    parent_id   UUID REFERENCES tags(id),  -- 层级嵌套
    color       TEXT,                      -- 色标 (#RRGGBB)
    sort_order  INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE asset_tag (
    asset_id    UUID REFERENCES assets(id),
    tag_id      UUID REFERENCES tags(id),
    PRIMARY KEY (asset_id, tag_id)
);
```

### 层级嵌套

```
摄影
├── 风景
│   ├── 日落
│   └── 海景
├── 人像
│   └── 街拍
└── 静物
```

- 支持无限层级
- 大小写不敏感
- 计数自动包含子树（标签旁显示 `(+3)` 表示含子标签的资产数）

### 色标

- 每个标签可设置颜色
- 在标签面板和资产网格中可视化
- 支持按色标筛选

### 标签操作

| 操作 | 说明 |
|------|------|
| 创建 | 面板 + 按钮，指定名称/父级/色标 |
| 重命名 | 级联更新所有关联 |
| 删除 | 级联移除关联 |
| 合并 | 将源标签合并到目标标签 |
| 拖拽 | 拖拽标签到资产上快速标记 |

### 快速标签

- 键盘快捷键调出标签面板
- 选中文材后批量输入
- 自动去重

### 标签筛选

- 工具栏标签筛选面板
- 点击标签 → 筛选显示含该标签的资产
- 支持多选（and/or 组合）
- 计数显示匹配资产数

---

## 合集 (Collections)

### 数据模型

```sql
CREATE TABLE collections (
    id          UUID PRIMARY KEY,
    name        TEXT NOT NULL,
    parent_id   UUID REFERENCES collections(id),
    cover_id    UUID REFERENCES assets(id),
    sort_order  INTEGER NOT NULL DEFAULT 0,
    created_at  TIMESTAMP NOT NULL
);

CREATE TABLE asset_collection (
    asset_id      UUID REFERENCES assets(id),
    collection_id UUID REFERENCES collections(id),
    sort_order    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (asset_id, collection_id)
);
```

### 功能特性

| 功能 | 说明 |
|------|------|
| 嵌套文件夹 | 无限层级 |
| 多对多归属 | 一个资产可属于多个合集 |
| 拖拽改父级 | 资源管理器中拖拽排序 |
| 循环检测 | 防止父级成为自己的后代 |
| 封面设置 | 自定义或自动首张 |
| 排序 | 自定义排序 / 按名称/日期 |
| 计数 | 显示子集内资产数 |

### 外观（图标与配色）

合集与智能合集共用同一份外观：`collections.appearance` / `smart_collections.appearance`
两个可空 JSON 列，`NULL` 就是默认样子。入口在合集树右键菜单的「图标与颜色」子菜单 ——
选择器就在菜单里展开，点一下立即写库、行随之改变，没有「确定」，也没有对话框。
新建智能集合时规则编辑器右栏也是同一份选择器，那里是草稿，保存时才写。

| 项 | 存什么 | 为什么 |
|----|--------|--------|
| 图标 | Emoji 字面量，或 Lucide 图标名（`palette`） | 名字而非字节：图标换库不搬库 |
| 配色 | 8 个色名之一（`cyan`…） | 同一个 hex 不可能在亮暗两套主题里都读得对，存名字让主题各自决定 |

- 强调色只作为**前景**使用（图标与行文字），不做底色，因此不存在“配了色还要再推一个可读前景”。
- Emoji 由系统彩色字体绘制，不接受着色 —— Emoji 段末尾会写明这点，只有名称吃强调色。
- 目录外的图标名（旧库、手改）退化成默认图标，不会让合集打不开。
- 目录里的图标必须登记进 `crates/trove-app/src/assets.rs` 的 `ExtraIcons`：gpui-kit 默认只嵌入约 100 个图标，未登记的会画成透明。`every_appearance_icon_is_bundled` 测试就是守这条的。

### 合集操作

- **新建**：按钮或右键菜单
- **重命名**：双击或 F2
- **删除**：移入回收站（仅移除组织关系）
- **拖拽添加**：从工作区拖拽资产到合集
- **排序**：自定义拖拽排序
- **外观**：右键菜单「图标与颜色」，见上

---

## 智能合集 (Smart Collections)

### 概念

规则驱动的虚拟文件夹 — 不手动维护，按条件动态归集符合条件的资产。

### 数据模型

```sql
CREATE TABLE smart_collections (
    id          UUID PRIMARY KEY,
    name        TEXT NOT NULL,
    parent_id   UUID REFERENCES collections(id),
    query_json  TEXT NOT NULL,  -- JSON 查询树
    sort_order  INTEGER NOT NULL DEFAULT 0
);
```

### 查询规则

| 字段 | 操作符 | 说明 |
|------|--------|------|
| `rating` | `>=`, `=`, `<=`, `>` | 评分 |
| `kind` | `is`, `is_not` | 资产类型 |
| `text` | `contains`, `starts_with` | 文件名/标题/描述 |
| `tag` | `contains`, `not_contains` | 标签（含子树） |
| `favorite` | `is` | 收藏状态 |
| `color` | `similar_to` | 颜色相似度 |
| `date` | `before`, `after`, `between` | 拍摄日期 |
| `aspect_ratio` | `is` | 宽高比 |
| `orientation` | `is` | 横/竖/方 |
| `width` | `>`, `<`, `=` | 宽度 |
| `height` | `>`, `<`, `=` | 高度 |

### 组合逻辑

```json
{
  "operator": "and",
  "conditions": [
    { "field": "rating", "op": ">=", "value": 4 },
    { "field": "kind", "op": "is", "value": "image" },
    { "operator": "or", "conditions": [
      { "field": "tag", "op": "contains", "value": "精选" },
      { "field": "color", "op": "similar_to", "value": "#FF5733" }
    ]}
  ]
}
```

- `and` / `or` 嵌套组合
- 编译期校验（无效字段/操作符拒绝保存）
- 标签规则自动包含子标签

### 示例智能集合

| 名称 | 规则 |
|------|------|
| 精选图片 | `rating >= 4 AND kind = image` |
| 未标记 | `tags count = 0` |
| 风景精选 | `tag contains 风景 AND rating >= 3` |
| 竖屏视频 | `orientation = portrait AND kind = video` |
| 暖色调 | `color similar to warm palette` |

---

## 批量操作工具栏

选中素材后，底部工具栏集中操作：

| 按钮 | 功能 |
|------|------|
| ⭐ | 收藏/取消收藏 |
| 📊 | 评分 (1-5 星) |
| 🏷️ | 标签（批量添加/移除） |
| 📝 | 备注（标题/描述/来源） |
| 🗑️ | 删除（入回收站） |
| 📁 | 添加到合集 |
| 📤 | 导出 |

### 批量标签

- 选中多个资产 → 点击标签按钮
- 输入标签名 → 批量添加
- 已有标签 → 显示勾选状态，可切换

### 批量评分

- 选中多个资产 → 点击评分按钮
- 选择星级 → 应用到所有选中

### 批量备注

- 编辑标题（统一或逐个）
- 编辑描述
- 编辑来源 URL

---

## 收藏 (Favorites)

- 一键收藏/取消收藏
- 收藏状态持久化
- 资源管理器"收藏"虚拟文件夹快速访问
- 智能集合可按收藏状态筛选

---

## 最近查看

- 左栏侧边最近 200 个资产
- 按查看时间倒序
- 入回收站自动隐藏，恢复后回归

---

## 代码位置

| 文件 | 内容 |
|------|------|
| `trove-core/src/store/tags.rs` | 标签 CRUD、层级、色标 |
| `trove-core/src/store/collections.rs` | 合集 CRUD、嵌套、封面 |
| `trove-core/src/store/smart_collections.rs` | 智能集合持久化 |
| `trove-core/src/model/tag.rs` | 标签数据模型 |
| `trove-core/src/model/collection.rs` | 合集数据模型 |
| `trove-core/src/model/smart_query.rs` | 查询树编译与执行 |
| `trove-app/src/panels/tags_panel.rs` | 标签面板 UI |
| `trove-app/src/panels/explorer.rs` | 资源管理器（合集树） |
| `trove-app/src/panels/workspace/toolbar/selection.rs` | 批量操作工具栏 |
| `trove-app/src/dialogs/edit.rs` | 批量像素编辑 |
| `trove-app/src/dialogs/convert.rs` | 批量格式转换 |
