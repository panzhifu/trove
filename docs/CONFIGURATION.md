# 配置管理 (Configuration)

> 应用配置、库配置、快捷键、多语言 — 个性化设置全解

---

## 概述

Trove 的配置分为两个层级：

| 层级 | 文件 | 作用域 |
|------|------|--------|
| 应用配置 | `config.json` | 全局（所有库共享） |
| 库配置 | `<library>/library.json` | 单个素材库 |

```
~/.config/trove/
├── config.json          # 应用配置
├── keybindings.json     # 快捷键
└── themes/              # 主题

Linux 路径（XDG 规范；macOS / Windows 用各自的 `data` / `cache` / `config` 目录）：

```
~/.config/trove/                   # 设置、主题、历史
~/.local/share/trove/
├── libraries/<slug>/
│   ├── library.db                 # 数据库
│   ├── library.json               # 库配置（监视文件夹等）
│   └── backups/                   # 每日数据库快照
└── incoming/                      # 截图与浏览器扩展收到的文件
~/.cache/trove/libraries/<slug>/   # 缩略图、全文索引（可随时删除重建）
~/.local/state/trove/logs/         # 日志
```
```

---

## 应用配置 (config.json)

### 结构

```json
{
  "libraries": [
    { "slug": "default", "name": "My Library" }
  ],
  "active_library": "default",
  "language": "zh-CN",
  "keybindings": { "OpenPreview": "enter" },
  "grid_zoom": 1.0,
  "undo_cap": 100,
  "filter_tools": ["type", "rating", "tags", "color"],
  "collect_enabled": true,
  "collect_port": 23916,
  "point_enhance": true,
  "appearance": "System",
  "theme_light": null,
  "theme_dark": null,
  "min_preview_zoom": 0.1,
  "max_preview_zoom": 10.0,
  "disabled_plugins": [],
  "plugin_settings": {
    "sidecar-notes": { "mode": "override" }
  },
  "embedding": {
    "base_url": "https://api.openai.com/v1",
    "api_key": "",
    "model": "text-embedding-3-small"
  }
}
```

### 配置项详解

`keybindings` 这张表的**键是动作名本身**（`"OpenPreview"`、`"MoveLeft"`…，值才是键串，
如 `"ctrl-p"`）—— 见 `crates/trove-core/src/config.rs:46`。上一版这里写的
`"workspace/open"` 不存在；同一版还在这个 JSON 末尾列了一行 `"watch_folders"`，而
监视文件夹既不是 `AppConfig` 的字段、也不叫这个名字（它在 `library.json` 里，键名
`watched_folders`，见下一节）。

#### 界面

| 键 | 类型 | 默认值 | 说明 |
|----|------|--------|------|
| `language` | `string?` | `null`（跟随系统） | 界面语言 |
| `appearance` | `enum` | `System` | `Light` / `Dark` / `System` |
| `theme_light` | `string?` | `null` | 浅色主题名 |
| `theme_dark` | `string?` | `null` | 深色主题名 |
| `grid_zoom` | `float?` | `1.0` | 网格缩放倍率 |
| `filter_tools` | `string[]?` | 默认集 | 工具栏筛选工具 |

#### 行为

| 键 | 类型 | 默认值 | 说明 |
|----|------|--------|------|
| `undo_cap` | `int?` | **`20`** | 撤销历史深度，读的时候 `clamp(1, 500)`（`config.rs:890`，默认来自 `DEFAULT_UNDO_CAP = 20`）。上一版这里写的 `100` 是示例值被当成了默认值 |
| `min_preview_zoom` | `float?` | `0.1` | 最小预览缩放 |
| `max_preview_zoom` | `float?` | `10.0` | 最大预览缩放 |
| `point_enhance` | `bool?` | `true` | 点云增强（EDL） |

#### 采集服务

| 键 | 类型 | 默认值 | 说明 |
|----|------|--------|------|
| `collect_enabled` | `bool?` | `true` | 是否启用本地采集 |
| `collect_port` | `int?` | `23916` | 采集服务端口 |

#### AI 嵌入

| 键 | 类型 | 默认值 | 说明 |
|----|------|--------|------|
| `embedding.base_url` | `string` | `""` | 嵌入 API 地址 |
| `embedding.api_key` | `string` | `""` | API Key |
| `embedding.model` | `string` | `""` | 模型名称 |

#### 插件

| 键 | 类型 | 默认值 | 说明 |
|----|------|--------|------|
| `disabled_plugins` | `string[]` | `[]` | 禁用插件列表 |
| `plugin_settings` | `object` | `{}` | 插件专属设置 |

---

## 库配置 (library.json)

每个库一份，跟着库走：换一台机器只要带走这个目录，这三类偏好都在。

### 结构

```json
{
  "watched_folders": ["/path/to/folder1", "/path/to/folder2"],
  "watch_folders_enabled": true,
  "purge_delete_sources": false,
  "skip_purge_confirm": false,
  "search_history": ["tag:猫", "sunset"]
}
```

### 配置项

五个键全部可缺省（`#[serde(default)]`），缺省即下表"默认"那一列。写出去的文件
会把每个字段都带上（`Option` 没有 `skip_serializing_if`，未设置就是 `null`），所以
读的一侧靠 `#[serde(default)]` 而不是靠键存在 —— 旧版本写的 `library.json` 少了后来
新增的键，照样能载入。

| 键 | 类型 | 默认 | 说明 |
|----|------|------|------|
| `watched_folders` | `string[]` | `[]` | 监视文件夹列表；出现在其中的文件自动导入，且是"未归档"（unfiled）。空数组 = 不监视 |
| `watch_folders_enabled` | `bool?` | `true` | 监视的总开关。关成 `false` 只是暂停监视，**列表原样保留**，再打开就继续 |
| `purge_delete_sources` | `bool?` | `false` | 彻底删除（purge）时，是否连带删掉磁盘上被链接的源文件。默认关：文件是用户自己的，放在哪儿都不管。收件箱里被 Trove 自己收集的文件不受这项影响，始终随记录一起删 |
| `skip_purge_confirm` | `bool?` | `false` | 是否跳过不可逆删除前的确认对话框。默认 `false` = 每次都问。用户可以在确认对话框里勾"不再询问"把它置 `true`，也可以在设置页"文件 → 删除"里关回来 |
| `search_history` | `string[]` | `[]` | 该库的最近搜索，最新在前，最多 24 条（`SEARCH_HISTORY_LIMIT`）。只在按回车提交时记录，输入到一半的前缀不进 |

`watched_folders` 早先在这份文档里写作 `watch_folders`，那是错的：结构体的字段名
`watched_folders` 就是 JSON 键名（没有任何 `#[serde(rename)]`）。

被监视文件夹跳过哪些内容不是配置项：文件夹自己的 `.gitignore` / `.ignore` /
`.git/info/exclude` 说了算，规则同 Git —— 见 [TASKS-BACKGROUND.md](TASKS-BACKGROUND.md) 的「忽略规则」。

读写都在 `trove-core/src/config.rs` 的 `LibraryConfig`（字段 `:448`、`skip_purge_confirm()`
`:525`）。

---

## 撤销历史 (`undo_log`)

在库数据库里，一张 `undo_log` 表（schema **v24** 加的），**一行一条已经生效的可逆改动**：

| 列 | 意思 |
|----|------|
| `seq` | 自增，就是历史里的位置 |
| `action` / `target` / `count` | 状态栏那行文案的三个来源（动作 slug、对象名、对象数） |
| `op` | `history::undo::Op` 的 JSON——逆向配方 |
| `created_at` / `undone_at` | 写下时间；`undone_at` 非空表示这条已被撤销、可以重做 |

`undone_at` 那一列就是撤销/重做的分界线，代替了早先内存里那两个 `Vec`：
"下一条要撤销的"= 最新的 `undone_at IS NULL`，"下一条要重做的"= **最早的** `undone_at IS NOT NULL`
（重做必须按正序回放，反着来会把数据库放进一个从未存在过的状态）。

三条行为值得知道：

- **撤销跨重启，重做不跨。** 开库时 `open_session` 删掉已被撤销的行——重启后重新做一个上一会话
  已经反悔掉的改动，不该在没人记得为什么的情况下自动发生。
- **记不上行，改动就不发生。** 13 个可撤销的入口都是"开事务 → 改 → 在同一事务里写行"，
  所以不存在"改了但没法撤，而且界面不说"这种状态。
- **读不回来的行在开库时删掉**（未知的 `action` slug、解析不了的 `op`），并 `tracing::warn!`
  点名 `seq`。不删的代价是 Ctrl+Z 从此报错卡住；乱猜的代价是撤销一次没人描述过的改动。

**只有数据库状态的改动在表里**（元数据 patch、回收站/收藏/标题批量翻、标签组替换、标签改名/色/父、
合集改名/移动/成员增减）。删除与彻底删除、导入、转换、批量改像素这类要动文件的，从来没被记过，
现在也不会——`Op` 里没有它们，表也就没有。深度由 `undo_cap` 控制（见上），超出cap的旧行删除。


---

## 快捷键

### 默认快捷键

唯一的真值是 `crates/trove-core/src/keybindings.rs` 的 `default_keybindings()`，现在 **24 条**（其中 4 条默认无键：`BatchRename` / `BatchConvert` / `AutoTag` / `Screenshot`，只能在菜单里用或在本页自己绑），下表逐条照抄（**上一版这张表编造了十几个不存在的绑定** —— `Shift+Delete` 永久删除、`F` 收藏、`1-5` 评分、`T` 标签面板、`Ctrl+F` 搜索、`Ctrl+C` 复制、`F11` 全屏、`Ctrl+=` / `Ctrl+-` / `Ctrl+0` 缩放、`Ctrl+]` / `Ctrl+[` 密度，全都没有注册过）。

| 操作 | 默认键 | 生效上下文 |
|------|--------|-----------|
| `MoveLeft` / `MoveRight` / `MoveUp` / `MoveDown` | `←` `→` `↑` `↓` | Workspace |
| `OpenPreview` | `Enter` | Workspace |
| `QuickLook` | `空格` | **AssetGrid** |
| `TrashSelected` | `Delete`（`Backspace` 是固定别名，不可改） | Workspace |
| `SelectAll` | `Ctrl+A` | Workspace |
| `ClearSelection` | `Esc` | Workspace |
| `Undo` / `Redo` | `Ctrl+Z` / `Ctrl+Shift+Z` | Workspace |
| `CopyImage` | `Ctrl+Shift+C` | Workspace |
| `ImportFiles` | `Ctrl+O` | 全局 |
| `OpenSettings` | `Ctrl+,` | 全局 |
| `RefreshLibrary` | `F5` | 全局 |
| `PasteImport` | `Ctrl+Shift+V` | 全局（写死在 `main.rs`，不在可配表里） |
| `EnterVideoFullscreen` / `ExitVideoFullscreen` | `F` / `F` | VideoPreview / VideoFullscreen |
| `StepFrameBack` / `StepFrameForward` | `,` / `.` | VideoPreview（视频与动图共用，见下） |
| `BatchRename` / `BatchConvert` / `AutoTag` | **默认无键** | Workspace（菜单里，可在本页自行绑） |
| `Screenshot` | **默认无键** | 全局（同上） |

`StepFrameBack` / `StepFrameForward` 走 `,` / `.`：逐帧步进，**按下先停住播放**，再走一帧留在那一帧上（挑封面帧用的，边播边跳对不准）。动图两个方向都会绕回循环，视频则钳在 0 与片长之间。它和 `TogglePlayback`、`f` 一样绑在 `VideoPreview`，所以只在预览占屏时生效。

**键名要写成平台会上报的那个字符串。** `Keystroke::parse` 把非修饰符的那一段**原样**当作键名，而 gpui-linux 是先查一张 keysym 表再落回 `keysym_get_name(...).to_lowercase()`：**有字符的键上报字符**（`Keysym::comma` → `","`、`Keysym::period` → `"."`），没字符的才上报名字（`escape` / `space` / `left`）。所以 `"comma"` 这种写法能解析、能在本页显示得像绑好了、却永远打不中——`OpenSettings` 就是这么带着一个死的 `Ctrl+,` 上线的，直到 09-29 被一条测试抓住（`app/keybindings.rs` 的 `every_default_binding_names_a_key_the_platform_can_emit` 钉住整张表）。

`QuickLook` 是唯一一个绑在 **AssetGrid** 而不是 Workspace 的：搜索框也在 `Workspace` 上下文里，而 gpui 只在按键事件仍向上传播时才把字符交给聚焦的输入框，所以绑在那边的裸字符键会让搜索框打不出那个字符——空格尤其致命。

### 自定义快捷键

在"设置 → 快捷键"里改，或直接写 `config.json` 的 `keybindings`：**键是动作名本身**（不是 `workspace/open` 这种路径式 id），值是键串；空串表示解绑。

```json
{
  "keybindings": {
    "OpenPreview": "enter",
    "QuickLook": "space",
    "SelectAll": "ctrl-a",
    "TrashSelected": "delete",
    "BatchRename": "f2"
  }
}
```

### 快捷键格式

| 格式 | 示例 | 说明 |
|------|------|------|
| 单键 | `enter`, `delete`, `f` | 无修饰键 |
| 组合键 | `ctrl-a`, `ctrl-shift-z` | 多修饰键 |
| 特殊键 | `space`, `tab`, `escape` | 功能键 |
| 媒体键 | `media-play-pause` | 多媒体键 |

---

## 多语言

### 支持的语言

| 代码 | 语言 | 状态 |
|------|------|------|
| `en` | English | ✅ |
| `zh-CN` | 简体中文 | ✅ |
| `ja` | 日本語 | ✅ |
| `ko` | 한국어 | ✅ |
| `es` | Español | ✅ |
| `fr` | Français | ✅ |
| `de` | Deutsch | ✅ |
| `pt` | Português | ✅ |
| `ru` | Русский | ✅ |

### 语言切换

- 设置 → 外观 → 语言
- 实时切换，无需重启
- `null` 或 `System` 跟随操作系统

### 翻译文件结构

```
trove-app/src/i18n/
├── en.toml
├── zh-CN.toml
├── ja.toml
└── ...
```

```toml
# zh-CN.toml
app_title = "Trove"
welcome = "欢迎使用 Trove"
library = "素材库"
settings = "设置"
```

---

## 外观设置

### 主题

| 主题 | 说明 |
|------|------|
| System | 跟随系统深浅色 |
| Light | 强制浅色 |
| Dark | 强制深色 |

### 密度

| 密度 | 说明 |
|------|------|
| 紧凑 | 最小间距，最多内容 |
| 默认 | 平衡间距 |
| 舒适 | 较大间距 |

### 网格缩放

- 滑杆范围：0.5× – 2.0×
- 影响缩略图大小和行高
- 工具栏滑杆 + 快捷键 `Ctrl+]` / `Ctrl+[`

---

## 文件设置

设置页「文件」这一页的组，按页内顺序（代码在 `crates/trove-app/src/dialogs/settings/files.rs`）。
**没有"导入方式"这个开关**：Trove 的导入语义是"只链接"，`AppConfig` / `LibraryConfig` 里都没有
复制/移动的选择项，历史上这份文档里的这张表是凭空画的。

| 组 | 里面有什么 |
|----|------------|
| 素材库 | 库名称（可改）；当前库所在目录只给"打开所在文件夹"，不把路径写死在界面上 |
| 监视文件夹 | 添加/移除目录、总开关。新文件自动入库且**不归入集合**；跳过哪些内容由文件夹自己的 `.gitignore` 决定 |
| 存储占用 | 库目录 / 缓存目录各占多少，只读 |
| 缩略图 | 音频卡片用封面还是波形（`audio_card_style`）、重建缩略图、重读元数据（`remine`）。**没有质量与尺寸滑杆**——缩略图尺寸是写死的规则，不是设置项 |
| 备份 | 立即拍一张快照、打开 `backups/`，以及**从快照还原**（见下） |
| 清理与校验 | 清理孤儿文件、完整性校验（逐个 blob 对哈希，缺失即入回收站） |
| 删除 | 两颗开关：彻底删除时是否连带删源文件（`purge_delete_sources`）、永久删除前是否询问（`skip_purge_confirm` 的反向读法） |
| 状态 | 无标题的一行，显示后台任务/健康状态 |

### 备份与还原

库目录下 `backups/` 存的是 `VACUUM INTO` 出来的**数据库快照**，最多 10 份（滚动删除最旧的），
开库时若最新一份已超过 24 小时会自动补一张。代码在 `crates/trove-core/src/services/backup.rs`。

还原（`restore_backup`，`services/backup.rs:115`）的四条性质，界面上的确认框也这么说：

- **快照只含记录。** 媒体文件在库目录里另放、缩略图与全文索引在缓存目录，都不在快照里。所以
  快照之后被删掉的文件**不会回来**（还原出的记录会读成"文件缺失"，完整性校验正是报这个的），
  而快照之后新增的文件**不会被删**（只是暂时没人记录它）。
- **写回不动文件，也不需要先关库。** 走的是 SQLite 在线备份，目的地是库文件本身的一条新连接，
  当前正被界面读着的那个句柄仍旧看见旧页——所以还原之后紧接着重开库并清掉所有算过的状态。
- **还原之前先拍一张。** 返回的路径就是"还原前的状态"，因此这一步本身可以退回：再还原那份文件即可。
- **预检在先。** 不是数据库、没有 `assets` 表、或 `user_version` 比本程序支持的（现在 v23）更高，
  三种都在写第一个字节之前拒绝掉——后者若放行会得到一个打不开的库。

还原之后全文索引必然与库内容对不上（索引记的是"哪些行存在"），界面会自动跑一次重建；缩略图不用管，
它们按内容哈希寻址、未命中当场补。整库导出（连媒体一起的 ZIP，`services/archive.rs`）**仍然只能手工
解压覆盖**，那一侧没有导入函数。

---

## 搜索设置

### 全文搜索

| 设置 | 说明 |
|------|------|
| 索引重建 | 手动重建搜索索引 |
| 中文分词 | 启用 jieba 分词 |
| 拼音搜索 | 启用拼音匹配 |
| 模糊匹配 | 启用容错搜索 |

### AI 嵌入

| 设置 | 说明 |
|------|------|
| 端点 URL | OpenAI 兼容 API 地址 |
| API Key | 认证密钥 |
| 模型名称 | 嵌入模型名 |
| 测试连接 | 验证配置 |
| 手动回填 | 触发嵌入计算 |

---

## 设置页结构

八页，按侧栏顺序（`dialogs/settings/mod.rs:417-428` 的 `.page(...)` 链，侧栏索引
`SettingsPage::index` 必须跟它一致；默认落在"关于"）。页与分组的标题就是下面这些
`settings.*` 键，取的是 `en.toml` / `zh-CN.toml` 的实际文案：

```
设置
├── 关于            （版本信息、自动更新检查、语言）
├── 外观            （基础 / 主题 / 自定义主题）
├── 文件            （素材库 / 监视文件夹 / 存储占用 / 缩略图 / 备份 /
│                     清理与校验 / 删除 / 状态）
├── 模型            （点云 / 预览缩放）
├── 搜索            （搜索层级 / 全文索引 / 视觉指纹）
├── AI 向量         （模型端点 / 向量库 / 分析模型 / AI 分析）
├── 快捷键          （筛选 + 键位列表 + 恢复默认）
└── 插件            （已安装的插件）
```

几点容易记错的：

- **没有"通用"这一页**。语言在"关于"里，深浅色与主题自成"外观"页，自动更新检查也在"关于"页。
- **"文件"页里没有"导入方式"这一组**：`crates/` 下 `ImportMode` / `import_mode` 零命中，
  两份配置里也没有对应字段——"导入时复制或移动"这个开关从来不存在。这一页列的是当前库、
  监视文件夹、存储占用、缩略图、备份、清理与校验、删除，最后一组是无标题的一行"状态"。
- **"删除"组**（`settings.deletion`）两颗开关：`永久删除时删除源文件`
  （`purge_delete_sources`）与 `永久删除前询问`（`skip_purge_confirm` 的反向读法），
  两者都写 `<library>/library.json`。
- 侧栏索引 0..6 对应前七页；"插件"页由 `plugins_page()` 排在最后，`SettingsPage`
  枚举里没有它（它不是任何入口的深链目标）。

---

## 配置迁移

### 版本升级

- 启动时自动迁移
- 新增配置项使用默认值
- 废弃配置项静默忽略

### 手动迁移

1. 导出配置：`config.json` + `keybindings.json`
2. 在新机器上放置到配置目录
3. 启动 Trove

---

## 代码位置

| 文件 | 内容 |
|------|------|
| `trove-core/src/config.rs` | 配置结构体、加载/保存 |
| `trove-core/src/paths.rs` | 路径计算 |
| `trove-core/src/keybindings.rs` | 快捷键解析 |
| `trove-app/src/app/i18n.rs` | 多语言支持 |
| `trove-app/src/app/theme.rs` | 主题管理 |
| `trove-app/src/app/actions.rs` | 操作定义 |
| `trove-app/src/dialogs/settings/mod.rs` | 设置对话框 |
| `trove-app/src/dialogs/settings/appearance.rs` | 外观设置 |
| `trove-app/src/dialogs/settings/files.rs` | 文件设置 |
| `trove-app/src/dialogs/settings/search.rs` | 搜索设置 |
| `trove-app/src/dialogs/settings/shortcuts.rs` | 快捷键设置 |
| `trove-app/src/dialogs/settings/plugins.rs` | 插件设置 |
| `trove-app/src/dialogs/settings/ai.rs` | AI 设置 |
| `trove-app/src/dialogs/settings/about.rs` | 关于页 |
