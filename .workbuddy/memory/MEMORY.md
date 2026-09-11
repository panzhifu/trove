# trove 项目长期记忆

## 提交与工作区约定
- 历史提交用 Conventional Commits（英文），粒度按功能拆：`feat(convert): …` / `feat(preview): …` / `feat(screenshot): …`。
- `.workbuddy/memory/*.md` 是**被 git 追踪**的，历史提交里和功能改动一起入库。
- `.zcode/plans/*.md` 是工具产物，未追踪，不要提交。
- 推送必须绕行 SSH（执行器以 root 运行、系统 ssh 配置不可读）：
  ```bash
  cd /home/noke/Code/trove && GIT_SSH_COMMAND="ssh -F /home/noke/.ssh/config -i /home/noke/.ssh/id_ed25519 -o IdentitiesOnly=yes -o UserKnownHostsFile=/home/noke/.ssh/known_hosts" git push origin main
  ```

## i18n（rust_i18n）
- 语言文件：`crates/trove-app/locales/{en,zh-CN}.toml`，**必须同键同序**（文件头注释明写）。
- 插值用 `%{var}`。新增代码里的 `t!("a.b")` 若没写进两个 toml，界面会原样显示 key —— 提交前务必跑键校验（tomllib 展开成点分扁平键后比对序列，见下）：
  ```bash
  python3 - <<'EOF'
  import tomllib
  def flat(d,p=""):
      out=[]
      for k,v in d.items():
          key=f"{p}{k}"
          out+=flat(v,key+".") if isinstance(v,dict) else [key]
      return out
  a=flat(tomllib.load(open("crates/trove-app/locales/en.toml","rb")))
  b=flat(tomllib.load(open("crates/trove-app/locales/zh-CN.toml","rb")))
  print(len(a),len(b),a==b,[k for k in a if k not in b],[k for k in b if k not in a])
  EOF
  ```
- `shortcuts.actions.<ActionId>` + `settings.rs::action_label` 是快捷键面板显示名的两处来源，新增可绑定 action 时都要加。

## gpui / gpui-kit 关键坑（本项目实测）
- **sprite atlas 永不淘汰**：`BladeAtlas::get_or_insert_with` 只插不删，`tiles_by_key` 无 LRU。任何「每帧新建 `RenderImage`」的动画/视频都会持续泄漏显存；换帧时必须 `window.drop_image(old_frame)`（`Window::drop_image` 会按 (image_id, frame_index) 删 tile）。只在 render 里做，因为那里才拿得到 `&mut Window`。
- `Context::spawn` 闭包签名是 `(WeakEntity<T>, &mut AsyncApp)`；`WeakEntity::update(cx, …)` 直接传 `cx`（签名是 `&mut C: AppContext`）。
- `SliderState::set_value(value, &mut Window, cx)` 是三参 —— 程序化同步滑块值只能放在 render（有 window），后台任务里改不了。
- `cx.background_executor().spawn(async move { … &var })` 会把 `var` move 进 future；外层还要用就先 `clone()`。
- `img()` 的 `ImageSource::Render(Arc<RenderImage>)` 可直接喂自建帧；BGRA 字节塞进 `image::RgbaImage::from_raw` 再 `image::Frame::new` 即可（`panels/common.rs` 的 APNG 与 `library/video_player.rs` 都这么做）。
- `Window::drop_image` **对所有「把自建帧塞进 img」的地方都要做**，不只是逐帧动画：`library/viewport3d.rs` 的视口在关闭/被替换时也必须归还最后一帧（`release(&mut Window)`），否则整屏一帧常驻 atlas。任何「能拿到 `&mut Window` 的退出路径」都要调用。
- 名字冲突：本项目 `trove_core::media::mesh::Bounds` 会遮住 gpui 的 `Bounds`，同文件里要 `use ... ::Bounds as MeshBounds`。
- `on_prepaint` 来自 `gpui_kit::base::ElementExt`，只挂在 `Div` 上，必须在 `.id()` **之前**调用（`.id()` 之后变 `Stateful<Div>`，方法不在了）。
- `CursorStyle` 里的手型是 `ClosedHand`（拖拽中）/ `OpenHand`（可拖拽），没有 `Grabbing`。
- `ScrollDelta` 两个变体：`Lines(Point<f32>)` 和 `Pixels(Point<Pixels>)`（后者要 `.as_f32()`）。
- `ClickEvent` 是 enum，双击次数用 `event.click_count()`，不是 `event.up.click_count`。
- **在面板模块里加测试不要 `use super::*`**：面板模块已经 glob 了 gpui prelude，会把 gpui 的 `test` 属性宏一起带进来，与标准 `#[test]` 冲突 → `recursion limit reached while expanding #[test]`。测试模块里显式写 `use super::{Cell, Row, …};`（`gpu3d.rs` 能用 glob 是因为那个模块没引 gpui prelude）。

## 主题与外观（gpui-kit 的 Theme）
- 主题是框架自己的：`Theme` global 存 `light_theme`/`dark_theme` 两个 `ThemeConfig` + 当前 `ThemeMode`；`ThemeRegistry` global 按名字存所有主题。切换 = 换 config + `Theme::change(mode, window, cx)` + `cx.refresh_windows()`。
- `crates/trove-app/build.rs` 用 `cargo metadata` 定位 gpui-kit 的 `themes/`，拷进 `OUT_DIR/themes` 并生成 `builtin_themes.rs`（`BUILTIN_THEME_JSON`）——**21 个文件共 36 个主题（25 dark / 11 light）**，框架默认只注册 Default Light/Dark 两个。`TROVE_THEMES_DIR` 可覆盖。
- 用户偏好存 `AppConfig`：`appearance`(system/light/dark) + `theme_light`/`theme_dark`(名字)；`app/theme.rs::apply_from_settings` 是唯一入口，启动时调一次，`AppView` 用 `window.observe_window_appearance` 订阅系统明暗。用户自定义主题放 `<config>/trove/themes/*.json`（`register_user_themes` 后加载，覆盖同名内置）。
- ⚠️ `apply_from_settings` 在 appearance 观察者里必须传 `Some(window)`：Linux 上 `cx.window_appearance()` 走的 `RefCell` 正被借用，直接查会 panic。

## 外部应用打开资产（Open With）
- `services/open_with.rs`：扫 `$XDG_DATA_HOME` + `$XDG_DATA_DIRS` 下所有 `.desktop`（含子目录），只要 `Type=Application`、非 NoDisplay/Hidden/Terminal、`MimeType` 命中就进候选；`Exec` 的字段码（`%f %F %u %U %i %c %k %%`）自己展开，无文件码时按规范补路径。目录里较早的 desktop-file id 优先，结果按「精确 mime 优先于 `type/*` 通配，再按名字」排序，进程内缓存（`reload_catalogue()` 可失效）。
- **内容寻址的 blob 不能就地编辑**：库内路径就是 SHA-256，就地写会破坏去重与完整性校验。所以 `library/open_with.rs` 对 `Origin::Stored` 先复制一份到 `<config>/edit/<asset_id>/<原文件名>` 再交给外部程序（副本存在就绝不覆盖，否则会吞掉用户的编辑），`edited_copy()` 判定「改过」后才在菜单里出现「导入编辑后的副本」（先比大小，≤32 MiB 才真去 hash，避免右键卡帧）。`Origin::Linked` 是用户自己的文件，直接原地打开。

## 三维模型（Model）资产
- `AssetKind::Model` 已存在；OBJ/STL/PLY 由 `media::mesh` 解析，缩略图走 `media::render3d`（CPU 光栅化），交互视口走 `library/gpu3d.rs`（自建 wgpu 设备 + 离屏渲染 + readback）。
- **CPU / GPU 必须共用 `render3d::Framing` 与 `VertexData`**，否则缩略图和视口的取景、着色会不一致。
- wgpu 29 是直接依赖（与 gpui 内置版本对齐，只有一个版本、一套后端）。`PipelineLayoutDescriptor` 无 `push_constant_ranges`（用 `immediate_size: 0`）、`bind_group_layouts: &[Option<&BindGroupLayout>]`、`DepthStencilState` 的 `depth_write_enabled`/`depth_compare` 是 `Option<_>`。
- **WGSL 可以在无 GPU 机器上校验**：`naga` 作为 trove-app 的 dev-dependency，测试里 parse + validate，并用 `naga::proc::Layouter` 断言 uniform 各成员 offset 与 Rust `Uniforms::to_bytes` 一致。改 shader 或改 uniform 结构后务必跑（`cargo test -p trove-app`）。

## 拆分脏工作区
- 通用按 hunk 暂存脚本：`/home/noke/.cache/trove-split/stage_hunks.py`（spec JSON，内部固定 `-U3` + `git apply --cached --recount`）。谓词要写窄，关键字命中的 hunk 常比预期多。
- ⚠️ `git stash push --keep-index --include-untracked` 遇到被占用文件会「只存不清理且返回非零」，`&&` 链断掉后 pop 不执行，再 drop 就丢文件。**验证中间提交用不带 `--include-untracked` 的版本**。

## 外部工具集成
- ffmpeg / ffprobe 是**可选运行时依赖**（不链接，仅执行），缺失时降级：视频缩略图退回图标、预览退回静态海报、视频探测退回 `media::probe::video_facts`（只读 MP4 moov，无帧率）。
- 截图同理：`services/screenshot.rs` 纯函数 `plan_for(mode, custom, dest, Platform)` 可单测（Platform 显式注入，避免测试里改 env）。
