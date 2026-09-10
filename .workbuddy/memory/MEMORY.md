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
- 插值用 `%{var}`。新增代码里的 `t!("a.b")` 若没写进两个 toml，界面会原样显示 key —— 提交前务必跑一次键校验（见 2026-09-10 日记录的 python 扫描脚本思路）。
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
