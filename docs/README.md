# Trove 文档索引

| 文档 | 内容 |
|---|---|
| [FEATURE-GAPS.md](./FEATURE-GAPS.md) | **功能差距分析**：对标 Eagle / Billfish / Pixcall / TagStudio / Allusion / digiKam / XnView MP / Adobe Bridge / IMatch / ACDSee / PhotoPrism / Immich 的完整功能对比，逐条标注优先级（P0–P3），并给出路线图建议。规划新功能从这里开始。 |
| [IMPORT-PIPELINE.md](./IMPORT-PIPELINE.md) | **导入管线的现状、实测与剩余优化**：管线形状（六阶段 + 一次解码 + 分窗口 + 两档池 + 子进程闸）、真实终端量到的数字（池宽扫描、stage/commit 组成、热重导入）、已做过的优化与提交、还能做的六项（含证据 file:line 与测法）、已到顶不要再动的清单、未标定常数一览。**要动导入性能或排查「导入慢」从这里开始。** |
| [MODEL-PREVIEW-ROADMAP.md](./MODEL-PREVIEW-ROADMAP.md) | **3D 模型预览优化路线图与交接文档**：目标与验收标准、GPU 环境阻塞项（NVIDIA 驱动版本不匹配）与重启验证清单、已完成部分（交互/EDL+补洞/背面剔除/大文件内存兜底/空间索引/GPU 后处理/meshlet 剔除，含实测数字）、需拍板的决策点、命令速查与已知的坑。**继续这项工作从这里开始。** |
| [MODEL-PREVIEW-BACKLOG.md](./MODEL-PREVIEW-BACKLOG.md) | **模型预览未做优化的存档**：被搁置的 VRAM 池分页、GPU 节点剔除与间接绘制、compute 光栅化、遮挡清理、网格分页、索引体积压缩、导入延迟哈希 —— 每一项的目标、设计要点、改动位置、依赖、验收与阻塞原因，以及明确不做的理由。想重启其中某项时看这里。 |
| [screenshots/](./screenshots/) | 界面截图（README 引用）。 |

历史文档 `FRONTEND-GAPS.md`（前后端能力对照）已于 2026-09-08 删除，其内容已被上述差距文档吸收替代。
