# 文档总览

`document/` 是 Suba 的设计文档。这里解释教学内核为什么这样分层、模块如何协作、哪些约束不能破坏。函数签名和局部实现细节放在 rustdoc 与源码注释里。

[架构综述](teaching_kernel_overview.md)说明教学目标、执行环境、文件服务和跨核复制。[裁剪与重构设计](teaching_kernel_design.md)对照需求与参考系统，给出模块方案和分阶段验收。[需求交接](teaching_kernel_requirements.md)列出项目目标、已确认方向和工作范围。[结构与运行规约](teaching_kernel_implementation.md)展开任务切换、请求交付和服务接入。

可运行版本的入口、平台限制、验收命令和行数预算见仓库根 README。原 Comix 的 Linux ABI 内核在 [基线提交 e2dc94b](https://github.com/comix-kernel/comix/tree/e2dc94bcb05d85e5748a2717af8d87714195eae1)。

## 文档分层

- `document/`：设计、边界、关键流程和不变量。
- `os/` rustdoc：公共类型、函数、错误条件和调用约束。
- 源码行内注释：unsafe 依据、锁顺序和非显而易见的局部逻辑。

## 写作约定

- 一篇设计文档说明当前状态、目标与非目标、模块边界、关键流程、并发或生命周期约束，以及已知限制。
- 源码路径使用仓库根相对路径，例如 `os/src/runtime.rs`。
- 文档只描述当前代码已经提供的能力。未实现能力标为限制或后续方向。
- 新增正式页面后更新 `document/SUMMARY.md`。

## 预览与构建

在仓库根目录执行：

```bash
mdbook serve
mdbook build
```

API 细节：

```bash
cd os
cargo doc --workspace --no-deps --target riscv64imac-unknown-none-elf
```
