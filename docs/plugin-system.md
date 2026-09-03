# WASM 插件系统设计

> 选型：**wasmtime + WIT 组件模型**（wit-bindgen 生成两侧绑定）。
> 插件接口采用**静态分发 trait**：宿主侧单态化，零 `dyn`、零 enum、运行时加载。

## 1. 核心思想：静态分发 trait × WASM

传统纯 Rust 插件若用 `dyn`（trait object）就是动态分发；若想静态分发，只能用 enum 枚举所有插件类型，于是宿主必须**编译时认识全部插件**。WASM 把这两件事解开了：

- **所有插件共享同一个宿主侧具体类型** `WasmFormatPlugin`，它持有一个 wasmtime `Instance` + 类型化的调用句柄。
- jxl 插件、png 插件……只是**不同的 `.wasm` 组件实例**，挂在同一个类型上。
- 于是注册表就是 `HashMap<String /*扩展名*/, Arc<WasmFormatPlugin>>` —— 零 `dyn`、零 enum、**运行时加载**，三者同时成立。

「静态分发 trait」在 WASM 时代的正确形态：**trait 方法在宿主侧被单态化为对组件实例的直接调用**，插件逻辑跑在沙箱里。

```rust
// trove-plugin-api：宿主侧 trait（单一事实源）
pub trait FormatPlugin: Send + Sync {
    fn extensions(&self) -> &[String];
    fn decode(&self, bytes: &[u8]) -> Result<DecodedImage, String>;
    fn extract_meta(&self, bytes: &[u8]) -> Result<ImageMeta, String>;
}

// trove-plugin-host：唯一的宿主侧具体实现，委托给 WIT 生成的绑定
pub struct WasmFormatPlugin {
    component: trove_plugin_api::format::Format, // wit-bindgen 生成的类型化句柄
    extensions: Vec<String>,
}
impl FormatPlugin for WasmFormatPlugin {
    fn decode(&self, bytes: &[u8]) -> Result<DecodedImage, String> {
        let out = self.component.call_decode(bytes)?; // 直接调用，无 dyn
        Ok(DecodedImage { data: out.data, width: out.width, height: out.height })
    }
    // extract_meta 同理
}
```

## 2. 接口定义（WIT，单一事实源）

WIT 放在 `trove-plugin-api/wit/`，插件（guest）与宿主（host）从同一份 WIT 生成绑定。

`format.wit`：

> WIT 的**唯一事实源**是 `crates/trove-plugin-api/wit/`（host 与 guest 都从它生成绑定）。
> 下面两段是它的副本，改动时请同步回源文件。

```wit
package trove:plugin@0.1.0;

/// RGBA 颜色。
record rgba {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

/// 解码后的 RGBA8 图像。
record decoded-image {
    /// RGBA8 像素，长度 = width * height * 4。
    data: list<u8>,
    width: u32,
    height: u32,
}

/// 图像元数据（建索引用，不参与渲染）。
record image-meta {
    width: u32,
    height: u32,
    color-space: option<string>,
    /// 预留：主色（v2 颜色筛选）。
    dominant-colors: list<rgba>,
}

interface format {
    /// 支持的文件扩展名（不含点，小写）。
    extensions: func() -> list<string>;
    /// 解码为 RGBA8。
    decode: func(bytes: list<u8>) -> result<decoded-image, string>;
    /// 提取元数据。
    extract-meta: func(bytes: list<u8>) -> result<image-meta, string>;
}

world format-plugin {
    export format;
}
```

`action.wit`：

```wit
package trove:plugin@0.1.0;

record action-result {
    success: bool,
    message: string,
    output: option<list<u8>>, // 压缩/转换后的字节
}

interface action {
    /// params 为 JSON 字符串（v1 简化；WIT 动态参数较繁琐）
    run: func(bytes: list<u8>, params: string) -> result<action-result, string>;
}

world action-plugin {
    export action;
}
```

要点：

- `list<u8>` 在组件边界自动落到线性内存（canonical ABI），由 wit-bindgen 生成，不用手写 ptr/len。
- 用 `result<_, string>` 承载错误，字符串为人类可读消息。
- `extract-meta` / `dominant-colors` 等可选能力，通过独立接口或 `option` 承载；宿主对缺失导出做降级。

## 3. 目录与依赖

```
trove/
├── crates/
│   ├── trove-core/          # 数据模型 + 仓库逻辑（不碰 GPUI）
│   ├── trove-plugin-api/    # host 侧 trait + WIT + wit-bindgen 生成的 host 绑定
│   ├── trove-plugin-host/   # wasmtime 加载、实例化、注册表、分发
│   └── trove-ui/            # GPUI + gpui-component
├── plugins/                 # 官方插件（各自构建为 .wasm 组件）
│   ├── format-jxl/
│   └── action-compress/
└── Cargo.toml               # workspace
```

依赖方向（自上而下，插件在最底）：

```
trove-ui → trove-plugin-host → trove-core
                            ↘ trove-plugin-api
plugins/* → trove-plugin-api（仅此一层，不链接进主程序）
```

## 4. 插件清单 `plugin.toml`

每个插件目录带一个清单，宿主据此加载：

```toml
# plugins/format-jxl/plugin.toml
[plugin]
id = "trove.format.jxl"
name = "JXL 格式"
kind = "format"            # format | action
version = "0.1.0"
entrypoint = "format_jxl.wasm"
```

`kind` 决定按哪个 WIT world 实例化组件；`entrypoint` 指向构建产物。

## 5. 加载与注册流程

```
1. 扫描 plugins/（内置）与用户插件目录
2. 读 plugin.toml → kind、entrypoint
3. 按 kind 用对应 world 实例化 .wasm 组件
   （wasmtime component model + Linker；v1 不注入任何 WASI 能力）
4. 调用 extensions() 拿扩展名 → 注册进 HashMap<ext, Arc<WasmFormatPlugin>>
5. 单个插件失败 → 记录日志并跳过，不影响主程序启动
```

## 6. 沙箱与安全

- **v1 零 WASI**：格式/动作插件是纯函数，不需要文件系统、网络、时钟，一个能力都不给。
- **资源限制**：wasmtime 的 fuel（指令预算）与 epoch（时间片）限制死循环；线性内存上限限制恶意分配。
- **插件不碰 DB / FS**：所有元数据、索引、缩略图编码都在宿主 core 完成，插件只有「字节进、数据出」。
- 未来若需要插件访问文件（如批量导入器），按最小能力单独授权 WASI，且单独 review。

## 7. 性能与内置解码

- **高频格式内置原生**：png / jpg 用宿主侧原生解码（`image` crate），不走 WASM。
- **WASM 兜长尾**：jxl / avif 等走插件，开 wasmtime SIMD，约原生 1.5–3 倍慢——缩略图是后台批量任务，感知不到。
- 缩略图只做两档（256 / 512），网格用 512 缩放；原图预览按需读原件。

## 8. 插件开发工作流

```bash
# 插件侧（cargo-component 构建）
cd plugins/format-jxl
cargo component build --release   # 产出 target/.../format_jxl.wasm
```

插件（guest）骨架：

```rust
struct JxlPlugin;
impl bindings::exports::trove::plugin::format::Guest for JxlPlugin {
    fn decode(bytes: Vec<u8>) -> Result<bindings::trove::plugin::format::DecodedImage, String> {
        // zune-jpegxl（纯 Rust）解码 → RGBA
    }
    fn extract_meta(bytes: Vec<u8>) -> Result<bindings::trove::plugin::format::ImageMeta, String> { /* ... */ }
}
```

> 具体绑定路径随 cargo-component / wit-bindgen 版本变化，以生成的 `bindings` 模块为准。

宿主侧绑定由 wit-bindgen 从同一份 WIT 生成，纳入 `trove-plugin-api` 的构建。
