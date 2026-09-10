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

## 外部工具集成
- ffmpeg / ffprobe 是**可选运行时依赖**（不链接，仅执行），缺失时降级：视频缩略图退回图标、预览退回静态海报、视频探测退回 `media::probe::video_facts`（只读 MP4 moov，无帧率）。
- 截图同理：`services/screenshot.rs` 纯函数 `plan_for(mode, custom, dest, Platform)` 可单测（Platform 显式注入，避免测试里改 env）。
