# 插件系统 (Plugin System)

> 可扩展的管线阶段、自定义命令、独立设置页 — 第三方能力接入

---

## 概述

Trove 的插件系统允许外部代码在不修改核心库的情况下扩展功能。当前版本（v1）提供以下扩展点：

| 扩展点 | 说明 |
|--------|------|
| 导入管线阶段 | 在导入流程中插入自定义处理阶段 |
| 键盘命令 | 注册可被绑定的快捷键命令 |
| 设置页 | 插件独立的配置页面 |
| 翻译文件 | 插件自己的多语言文本 |

---

## 插件架构

```
┌─────────────────────────────────────────────────────────────┐
│                         启动时                                │
│                                                             │
│  1. 加载 AppConfig（读取 disabled_plugins）                   │
│  2. 注册所有编译内置插件                                      │
│  3. 构建管线（内置阶段 + 启用的插件阶段）                       │
│  4. 注册命令和设置页                                          │
└─────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│                        插件运行时                             │
│                                                             │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────┐  │
│  │ PipelineStage│  │ PluginCommand│  │ SettingPage      │  │
│  │ (管线阶段)   │  │ (键盘命令)   │  │ (设置页)         │  │
│  └──────────────┘  └──────────────┘  └──────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

---

## Plugin Trait

```rust
pub trait Plugin: Send + Sync {
    /// 稳定标识符（kebab-case，如 "sidecar-notes"）
    fn name(&self) -> &'static str;

    /// 管线阶段（可选）
    fn pipeline_stages(&self) -> Vec<Arc<dyn Stage>> { vec![] }

    /// 键盘命令（可选）
    fn commands(&self) -> Vec<PluginCommand> { vec![] }

    /// 设置页（可选，AppPlugin 子 trait）
    fn settings_pages(&self) -> Vec<SettingPage> { vec![] }

    /// 翻译文件（可选，AppPlugin 子 trait）
    fn translations(&self) -> Vec<(&str, &str)> { vec![] }

    /// 命令处理
    fn run_command(&self, command: &str, window: &mut Window, cx: &mut App) {}
}
```

---

## 管线阶段扩展

### 插件阶段位置

```
[HashStage] → [ProbeStage] → [DecodeStage] → [ThumbStage] → [MineStage] → [VisualSigStage]
                                                                      │
                                                               [PluginStage]  ← 插件阶段追加于此
```

- 插件阶段在内置阶段**之后**运行
- 可以读取已完成的结果（如 mined metadata）
- 可以产生新的产出（但不能与内置阶段冲突）

### 编写管线阶段

```rust
use trove_core::media::pipeline::{Stage, StageIo, Need, Cost};

struct MyPluginStage;

impl Stage for MyPluginStage {
    fn name(&self) -> &'static str {
        "my-plugin"
    }

    // 可选：读取已解码的像素
    fn uses(&self) -> &'static [Need] {
        &[Need::Decode]
    }

    fn cost(&self) -> Cost {
        Cost::Cpu
    }

    fn run(&self, io: &mut StageIo) -> Result<()> {
        if let Some(decoded) = io.artifacts.get::<Decoded>() {
            // 使用解码后的像素...
        }
        Ok(())
    }
}
```

### 阶段依赖声明

```rust
impl Stage for MyEnrichmentStage {
    fn name(&self) -> &'static str {
        "my-enrichment"
    }

    // 需要 MineStage 已完成
    fn needs(&self) -> &'static [Need] {
        &[Need::Thumb]
    }

    fn cost(&self) -> Cost {
        Cost::Io
    }

    fn run(&self, io: &mut StageIo) -> Result<()> {
        // io.mined 已包含内置阶段的挖掘结果
        // 可以在这里补充额外元数据
        Ok(())
    }
}
```

---

## 键盘命令

### 定义命令

```rust
impl Plugin for MyPlugin {
    fn commands(&self) -> Vec<PluginCommand> {
        vec![PluginCommand {
            action: "my-plugin/do-something",  // 命名空间/动作
            key: "ctrl-shift-m",               // 默认快捷键
            global: false,                     // 是否全局生效
        }]
    }

    fn run_command(&self, command: &string, window: &mut Window, cx: &mut App) {
        if command == "my-plugin/do-something" {
            // 执行命令...
        }
    }
}
```

### 命令命名约定

- 格式：`"plugin-name/action-name"`
- 例：`"sidecar-notes/toggle-mode"`
- 唯一性：命名空间隔离避免冲突

### 快捷键绑定

- 用户在"设置 → 快捷键"中重新绑定
- 存储在 `AppConfig.keybindings` 中
- 空字符串表示未绑定

---

## 设置页

```rust
impl AppPlugin for MyPlugin {
    fn settings_pages(&self) -> Vec<SettingPage> {
        vec![
            SettingPage::new(pt!("plugins.my_plugin_name"))
                .description(pt!("plugins.my_plugin_desc"))
                .group(
                    SettingGroup::new()
                        .title(pt!("plugins.my_plugin_group"))
                        .item(
                            SettingItem::new(
                                pt!("plugins.my_plugin_option"),
                                SettingField::switch(
                                    |cx| cx.plugin_enabled,
                                    |value, cx| { cx.plugin_enabled = value; },
                                )
                            )
                        )
                )
        ]
    }
}
```

---

## 参考实现：Sidecar Notes 插件

### 功能

读取与源文件同名的 `.trove.json` sidecar 文件，将其中的 `title` 字段应用到资产。

```
IMG_0001.jpg       ← 源文件
IMG_0001.jpg.trove.json  ← sidecar
```

sidecar 内容：

```json
{ "title": "The cover shot" }
```

### 模式设置

| 模式 | 行为 |
|------|------|
| Override（默认） | sidecar 标题覆盖已有标题 |
| FillMissing | 仅当无标题时填充 |

### 管线阶段

```rust
impl Stage for SidecarNotesStage {
    fn name(&self) -> &'static str { "sidecar-notes" }
    fn cost(&self) -> Cost { Cost::Io }

    fn run(&self, io: &mut StageIo) -> Result<()> {
        let sidecar = sidecar_path(&io.src)?;
        let text = std::fs::read_to_string(&sidecar).ok();
        let value: serde_json::Value = serde_json::from_str(&text).ok();
        
        if let Some(title) = value.get("title").and_then(|v| v.as_str()) {
            let title = title.trim();
            if !title.is_empty() {
                match self.mode {
                    Mode::Override => { io.mined.title = Some(title.into()); }
                    Mode::FillMissing if io.mined.title.is_none() => {
                        io.mined.title = Some(title.into());
                    }
                }
            }
        }
        Ok(())
    }
}
```

### 命令

```rust
PluginCommand {
    action: "sidecar-notes/toggle-mode",
    key: "ctrl-alt-s",
    global: true,
}
```

### 翻译文件

```
plugins/sidecar-notes/
├── en.toml
└── zh-CN.toml
```

```toml
# en.toml
plugins.sidecar_notes_name = "Sidecar Notes"
plugins.sidecar_notes_description = "Read title from .trove.json sidecar files"
plugins.sidecar_notes_mode = "Mode"
plugins.sidecar_notes_mode_override = "Override"
plugins.sidecar_notes_mode_fill = "Fill Missing"
```

---

## 插件注册

### 编译内置

```rust
// plugins/builtin.rs
pub fn register_builtins(registry: &mut PluginRegistry) {
    registry.register(SidecarNotes::new());
    // registry.register(AnotherPlugin::new());
}
```

### 启用/禁用

- 配置：`AppConfig.disabled_plugins: Vec<String>`
- 设置页：插件列表中切换开关
- 生效时机：下次启动时重新构建管线

---

## 管线验证

插件阶段注册时会进行验证：

| 错误类型 | 说明 |
|---------|------|
| `DuplicateProducer` | 两个阶段产出同一个 Need |
| `NoProducer` | 需要某个 Need 但无默认产出者 |

验证失败的插件会被**跳过**（日志记录），不影响内置管线。

---

## 动态加载（规划中）

当前 v1 仅支持编译内置插件。下一步计划：

1. **API Crate** — 独立 `trove-plugin-api` crate，第三方可实现 Plugin trait
2. **动态加载** — `.so` / `.dll` 运行时加载
3. **沙箱** — 限制插件权限
4. **商店** — 插件发现和安装

---

## 代码位置

| 文件 | 内容 |
|------|------|
| `trove-core/src/plugins.rs` | Plugin trait、PluginRegistry、PluginCommand |
| `trove-app/src/plugins/builtin.rs` | 内置插件注册 |
| `trove-app/src/plugins/mod.rs` | 插件管理 |
| `trove-app/src/plugins/i18n.rs` | 插件翻译辅助 |
| `trove-app/src/plugins/locales/` | 内置插件翻译文件 |
| `trove-app/src/dialogs/settings/plugins.rs` | 插件设置页 |
