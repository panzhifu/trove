# 备份与维护 (Backup & Maintenance)

> 自动备份、回收站、完整性校验、XMP 导出、字体管理 — 数据安全保障

---

## 概述

Trove 提供多层次的数据保护机制：

| 机制 | 频率 | 说明 |
|------|------|------|
| 自动备份 | 每天 | SQLite VACUUM INTO 快照 |
| 回收站 | 即时 | 删除缓冲，可恢复 |
| 完整性校验 | 手动/维护 | SHA-256 重算比对 |
| 孤儿清理 | 维护任务 | 移除无引用 blob |
| 整包备份 | 手动 | zip 导出（配置+数据） |
| XMP 导出 | 手动/批量 | 标准 sidecar 元数据 |

---

## 自动备份

### 机制

使用 SQLite 的 `VACUUM INTO` 创建一致性快照：

```sql
VACUUM INTO '/path/to/backups/library_2026-01-15_143022.db';
```

### 策略

| 参数 | 值 | 说明 |
|------|-----|------|
| 频率 | 每天至多一次 | 避免频繁写入 |
| 保留 | 10 份 | 滚动删除最旧 |
| 位置 | `<library>/backups/` | 库目录内 |
| 命名 | `library_YYYY-MM-DD_HHMMSS.db` | 时间戳 |

### 备份内容

- ✅ 数据库快照（所有资产/标签/集合/元数据）
- ❌ 媒体 blob（可重新链接）
- ❌ 缩略图缓存（可重新生成）
- ❌ 搜索索引（可重建）

### 恢复

1. 关闭 Trove
2. 用备份文件替换 `library.db`
3. 重启 Trove

---

## 整包备份导出

### 导出内容

```
trove-backup.zip
├── manifest.json          # 恢复布局描述
├── config.json            # 应用配置
├── keybindings.json       # 快捷键
├── plugins/               # 插件设置
├── libraries/
│   └── <slug>/
│       ├── library.db     # 数据库快照
│       ├── library.json   # 库配置
│       └── incoming/      # 收件箱
```

### 导出方式

- 素材库管理器 → "导出备份压缩包"
- File 菜单 → "导出备份压缩包…"

### 恢复方式

1. 素材库管理器 → "从备份恢复"
2. 选择 zip 文件
3. 按 manifest.json 布局恢复

---

## 回收站

### 数据模型

```sql
ALTER TABLE assets ADD COLUMN deleted_at TIMESTAMP;
-- 删除时设置 deleted_at，而非物理删除
```

### 操作流程

```
用户删除
    │
    ▼
设置 deleted_at = now()
从搜索结果/集合中隐藏
    │
    ├──► 恢复 → 清除 deleted_at
    │
    └──► 永久删除 → 物理删除行 + blob
```

### 回收站功能

| 功能 | 说明 |
|------|------|
| 查看 | 资源管理器"回收站"文件夹 |
| 恢复 | 右键 → 恢复（保留组织关系） |
| 永久删除 | 右键 → 永久删除 |
| 清空回收站 | 一键清空，释放空间 |
| 自动隐藏 | 入回收站的资产从最近查看中移除 |

### 空间释放

清空回收站时：
1. 删除资产行
2. 删除关联的 blob 文件
3. 删除缩略图缓存
4. 清理嵌入向量

---

## 完整性校验

### 校验流程

```
1. 遍历所有资产记录
2. 对每个 blob 文件重算 SHA-256
3. 与记录的 sha256 比对
4. 不匹配 → 标记问题资产
5. 问题资产一键入回收站
```

### 触发方式

- 维护任务自动运行
- 手动触发：设置 → 维护 → 校验完整性

### 问题处理

| 问题 | 处理 |
|------|------|
| SHA-256 不匹配 | 标记损坏，建议入回收站 |
| 文件不存在 | 标记丢失，可重新链接 |
| 权限错误 | 跳过，记录日志 |

---

## 孤儿清理

### 概念

blob 文件不被任何资产引用 → 可安全删除

### 清理流程

```
1. 扫描 blob 目录中的所有文件
2. 与 assets 表中的 rel_path 比对
3. 无引用的文件 → 删除
4. 释放磁盘空间
```

### 安全保证

- 仅删除 `data/` 目录下的 blob
- 不触碰用户原始文件（链接导入）
- 维护任务中运行

---

## 重复文件查找

### 去重策略

| 层级 | 方法 | 说明 |
|------|------|------|
| 内容去重 | SHA-256 | 完全相同 |
| 视觉去重 | pHash | 视觉相同（缩放/压缩后） |

### 视觉去重流程

```
1. 按 pHash 聚类（汉明距离 ≤ 阈值）
2. 每组显示为重复组
3. 用户选择保留哪个
4. 其余入回收站
```

### 操作

- 工具 → 查找重复图片
- 按组浏览
- "保留最新，其余入回收站" 一键处理

---

## XMP 元数据导出

### 概念

将 Trove 的元数据写入标准 XMP sidecar 文件（`.xmp`），实现与其他软件的互操作。

### 导出内容

| 字段 | XMP 属性 |
|------|---------|
| 标题 | `dc:title` |
| 描述 | `dc:description` |
| 标签 | `dc:subject` |
| 评分 | `xmp:Rating` |
| 来源 | `dc:source` |

### 导出方式

- 单个资产：检查器 → 导出 XMP
- 批量：选中多个 → 右键 → 导出 XMP
- 自动：设置中启用"自动导出 XMP"

### 文件格式

```xml
<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description
      xmlns:dc="http://purl.org/dc/elements/1.1/"
      xmlns:xmp="http://ns.adobe.com/xap/1.0/">
      <dc:title>日落海滩</dc:title>
      <dc:description>长曝光拍摄</dc:description>
      <dc:subject>
        <rdf:Bag>
          <rdf:li>风景</rdf:li>
          <rdf:li>日落</rdf:li>
        </rdf:Bag>
      </dc:subject>
      <xmp:Rating>4</xmp:Rating>
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>
```

### 安全写入

- 原子写入（先写临时文件，再重命名）
- 全转义（防 XML 注入）
- 不破坏原文件

---

## 字体管理

### 功能

| 功能 | 说明 |
|------|------|
| 安装字体 | 安装到系统字体目录 |
| 卸载字体 | 从系统移除 |
| 预览字体 | 样张卡片 + 实况预览 |
| 字体信息 | 族名、样式、字重 |

### 平台差异

| 平台 | 字体目录 |
|------|---------|
| macOS | `~/Library/Fonts/` |
| Windows | `C:\Windows\Fonts\` |
| Linux | `~/.local/share/fonts/` |

---

## 存储统计

### 统计面板

```
存储占用
├── 设置与主题      2.3 MB
├── 数据库          156 MB
├── 备份            1.2 GB  [可删除]
├── 缩略图与索引    3.4 GB  [可重建]
├── 日志            45 MB   [可清理]
└── 收件箱          120 MB
```

### 可清理项

| 项 | 影响 |
|----|------|
| 备份 | 删除后无法从备份恢复 |
| 缩略图 | 删除后重新生成（耗时） |
| 搜索索引 | 删除后重建（耗时） |
| 日志 | 删除后丢失历史日志 |

---

## 代码位置

| 文件 | 内容 |
|------|------|
| `trove-core/src/services/backup.rs` | 自动备份 |
| `trove-core/src/services/archive.rs` | 整包备份导出/恢复 |
| `trove-core/src/services/maintenance.rs` | 维护任务（校验/清理） |
| `trove-core/src/services/storage.rs` | 存储统计 |
| `trove-core/src/services/xmp.rs` | XMP 导出 |
| `trove-core/src/services/font_manager.rs` | 字体安装/卸载 |
| `trove-app/src/dialogs/duplicates.rs` | 重复文件查找 |
| `trove-app/src/dialogs/settings/files.rs` | 文件设置页 |
