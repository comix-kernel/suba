# 文档总览

`document/` 是 Comix 的正式设计文档入口，面向内核贡献者和评审者。这里解释系统为什么这样分层、模块如何协作、哪些约束不能破坏；具体函数签名、字段语义和局部实现细节应优先维护在 rustdoc、模块注释和源码注释中。

[教学内核架构综述](teaching_kernel_overview.md)介绍教学目标、设计取舍，以及执行环境、文件服务和跨核复制之间的关系。[裁剪与重构设计](teaching_kernel_design.md)逐项对照需求与三个参考项目的源码，再给出整体架构、各模块的实现方案、源码取舍和分阶段验收。

接手教学内核工作时，先阅读[需求交接](teaching_kernel_requirements.md)，核对项目目标、已确认方向和当前工作范围。

[教学内核的结构与运行规约](teaching_kernel_implementation.md)用 Rust 展开任务切换、请求交付和服务接入，说明一次文件写入如何穿过各模块，以及替换期间由谁持有旧连接。

当前可运行版本的代码入口、平台限制、验收命令与行数预算见仓库根 README。导航中“历史基线”部分记录提交 `e2dc94b` 的 Linux 兼容内核。

## 文档分层

- `document/`：设计、边界、关键流程、不变量、已知限制和维护约束。已核对的逻辑错误集中记在 [known_bugs.md](known_bugs.md)，不要散落到各子系统概述里重复维护。排查进度和下一轮范围在 [bug_hunt_handoff.md](bug_hunt_handoff.md)。
- `os/src/**` rustdoc：公共类型、函数、错误条件、调用约束和小型示例。
- 源码行内注释：unsafe 依据、架构细节、锁顺序、性能取舍和非显而易见的局部逻辑。
- `local_doc/`：本地草稿和阶段性研究资料，不作为正式文档同步目标。

## 写作契约

- 每个子系统首选一个 `README.md` 做概览，子页面只覆盖清晰的设计主题。
- 一篇设计文档应说明当前状态、目标/非目标、模块边界、关键流程、并发或生命周期约束、已知限制和源码索引。
- 源码路径使用仓库根相对路径，例如 `os/src/ipc/pipe.rs`。
- 避免维护函数清单、字段大全、完整错误分支和长代码示例；这些内容更适合 rustdoc。
- 文档只描述当前代码已经提供的能力。未实现能力必须明确标为限制或后续方向。
- 新增正式页面后必须更新 `document/SUMMARY.md`，并在对应子系统 README 中加入导航。

## 导航策略

Comix 采用"细项主导航 + 简洁页面"的混合风格。主导航保留当前实现相关的细分设计页，方便读者直接跳到具体主题；页面内容则保持设计优先，避免膨胀成 API 手册。

- `document/SUMMARY.md` 可以列出有维护价值的细项，例如 MM 的地址/页表/地址空间、VFS 的 File/FDTable/路径挂载、IPC 的 pipe/shared memory/signal。
- 历史状态、未实现方案或重复 rustdoc 的页面不进入主导航，只从子系统 README 的历史或延伸阅读区域链接。
- 子系统 README 必须解释这些细项的阅读顺序和用途，避免主导航只是文件列表。
- 参考 SanktaOS 的短页面写法，但不照搬它的极简主导航。

## 预览与构建

在仓库根目录执行：

```bash
mdbook serve document -n 0.0.0.0 -p 4000
mdbook build
```

API 细节通过 rustdoc 生成：

```bash
cd os
cargo doc --workspace --no-deps --target riscv64imac-unknown-none-elf
```

## 维护流程

1. 先读最新源码入口和现有文档，确认文档要表达的是设计而不是代码搬运。
2. 在对应子系统目录更新 Markdown；跨子系统主题放到更高层级。
3. 更新 `document/SUMMARY.md` 和子系统 README 导航。
4. 运行 `mdbook build`，必要时运行 `python3 scripts/rewrite_links.py document/` 后再次构建。
5. 若改动涉及公共 API 文档，运行 `cargo doc --workspace --no-deps --target riscv64imac-unknown-none-elf`。

## 常见问题

- 新页面没有出现在左侧目录：需要加入 `document/SUMMARY.md`。
- 文档和 rustdoc 内容重复：正式文档保留设计说明，把 API 细节移回 rustdoc。
- 文档引用了不存在的源码路径：以 `os/src` 当前目录结构为准修正，不保留历史路径作为当前实现。
