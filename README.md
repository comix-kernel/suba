# Suba

Suba 是一个可以在 QEMU 中运行的双核 RV64 教学内核。它从 OpenSBI 进入 S 态，在一张 Sv39 页表上抢占调度 Rust 任务。shell、串口、内存文件和管理服务都通过有界的本机文件协议通信。每个 hart 有一份服务目录，打开的连接持有具体实例，发布新版本后旧连接仍可使用。

## 构建和运行

需要 rustup、仓库固定的 Rust nightly、GNU Make、Python 3、LLVM 的 `llvm-objcopy` 和 `llvm-objdump` 和带 OpenSBI 的 `qemu-system-riscv64`。在仓库根目录执行：

```sh
rustup target add riscv64imac-unknown-none-elf
make run
```

默认配置为 QEMU `virt`、128 MiB 内存、2 个 hart 和一个串口。固件启动后，每核打印实际根页号及 tick 数，然后显示应用菜单和 `suba>`。输入编号或程序名并按回车启动，`apps` 重看菜单，`help` 查内建命令，`halt` 关机。镜像和 ELF 位于 `target/teaching/riscv64imac-unknown-none-elf/release/`。内存文件在关机时清空。

## 第一次使用 shell

| 输入 | 运行结果 |
| --- | --- |
| `hello Mii` | 打印问候和 **hello 自己**的任务路径。用 `on 0 hello Mii` 可以在另一核启动它 |
| `ps` | 从 `/tasks/0`、`/tasks/1` 枚举任务，显示所属核、任务 ID 和采样状态 |
| `calc (7 + 5) * 3` | 打印 `36`。只输入 `3` 或 `calc` 可进入交互计算器，在 `calc>` 下输入 `q` 回到 shell |
| `put /work/note hello suba`，再 `wc /work/note` | 打印 `0 lines  2 words  10 bytes` 及文件名。行数按换行符统计，单独运行 `wc` 会读取 `/svc/memory` |

在 `suba>` 输入 `edit /work/note` 打开全屏串口编辑器，也可输入 `5 /work/note`。方向键移动，Home/End 到行首或行尾，Enter 换行，退格和 Delete 删除文字。**Ctrl-S 保存，Ctrl-Q 返回 shell**；有未保存修改时，再按一次 Ctrl-Q 放弃修改。编辑器支持 UTF-8，单文件上限 4 KiB。打开新的普通文件名会创建空文件，保存后的内容可用 `cat /work/note` 读取。文件保存在本次运行的内存中。

shell 和计算器支持退格、↑/↓ 回看最近八条输入、Ctrl-U 清行和 Ctrl-C 取消当前输入。程序预链接在镜像内，启动时创建独立的 Rust 任务，继承调用者的挂载视图。前台程序结束后，shell 取回串口输入权。[程序入口和菜单](os/components/src/programs/mod.rs)

## 在 shell 中观察架构

| 输入 | 可以观察到的结果 |
| --- | --- |
| `put /out/cons abcd`，再 `cat /out/count` | 计数服务向 UART 转发写入并累计已确认字节。普通 shell 提示符直接走串口服务 |
| `mount /dev record`，再 `put /dev/cons local`、`cat /dev/cons` | 以后打开的这个路径读写记录服务。提示符沿启动时已打开的串口句柄输出 |
| `id`，再对返回的路径执行 `cat` | 打开时采样该任务的 hart、槽号、代号和状态 |
| `open /work/version`、`replace`、`read`、`close`、`cat /work/version` | 首次运行依次读到旧实现 v1 和新实现 v2。并存切换允许两版数据独立演进 |
| `put /work/note hello`、`migrate`、`cat /work/note` | 新实例导入内容与修订号后才接入。持有未关的旧文件时迁移返回 Busy |

`replace` 和 `migrate` 写管理文件 `/svc/store/ctl`，当前代号可从 `/svc/store/generation` 读取。这个挂载也受服务接入授权约束。

### 复现组合、迁移和退役

在刚启动的 shell 里依次执行：

```text
create /work/chapter
put /work/chapter text
migrate
cat /work/chapter
on 0 mount /dev record; put /dev/cons child
put /svc/counter/ctl migrate record
put /out/cons xyz
on 0 mount /r record; cat /r/cons
cat /out/count
```

迁移后 `chapter` 仍含 `text`。子任务在核 0 继承挂载视图并改挂 `/dev`，父 shell 的 `/dev` 继续指向串口。计数服务迁移时将已确认总数带入新实例，通过注入的句柄转发到记录服务；最后两项输出为 `xyzld` 和 `3`。普通文件按偏移覆盖，短写保留后缀。

`ls /` 可浏览挂载视图，`ls /tasks/0` 在所属核列出任务。每个内存实例支持最多 16 个普通文件，包含独立的 `note` 和 `cons`。`/svc/{store,record,counter}/ctl` 支持 `publish`、`replace`、`migrate`、`withdraw`。内存服务可指定 `v1`（线性表）或 `v2`（有序索引），计数服务指定 `console`、`store` 或 `record` 为下游。控制 fid 固定打开当时的发布代号，过期命令返回 Conflict。查看同目录的 `status` 可以跟踪旧实例的连接、在途请求和 fid。

`/svc/memory` 显示可分配页帧数，`/svc/uart` 报告输入缓冲溢出，`/svc/replicas` 显示日志与两个游标。`/svc/trace/{0,1}` 保存各核最近 48 个事件，记录请求的实例、连接和 tag，以及任务等待序号。`make run TRACE=0` 关闭事件记录，`make check-teaching TRACE=0` 验收同一配置。

## 通读源码

| 入口 | 职责 |
| --- | --- |
| [启动](os/src/main.rs)、[平台](os/src/platform.rs)、[页帧池](os/src/frames.rs)、[链接布局](os/src/linker.ld) | 从 DTB 取得板级资源，安装带栈保护页的最终页表，启动第二个 hart |
| [调度器](os/src/runtime.rs)、[陷入汇编](os/src/trap.S) | 固定核轮转、时钟抢占、按事件阻塞、切栈与回收 |
| [文件协议和客户端](os/api/src/lib.rs)、[传输](os/src/transport.rs) | 挂载、接入能力、连接内 fid、有界队列和一次性回复 |
| [服务分发](os/components/src/server.rs)、[服务实现](os/components/src/lib.rs) | 检查访问模式，固定已打开的对象，将文件请求交给安全 Rust 组件 |
| [顺序目录](os/registry/src/lib.rs)、[复制驱动](os/src/registry.rs)、[实例管理](os/src/services.rs) | 纯值日志、写入合并、本核追赶、按版本接入、状态迁移与失败回退 |

首版支持 24 个任务槽，每个任务使用一个 64 KiB 栈。每个实例的请求队列容纳两条请求及合计 4 KiB 的已排队负载，单个内存文件最多 4 KiB。服务名和组件预编译到镜像中。替换回收任务、队列与状态，机器码随镜像驻留。[架构文档](document/teaching_kernel_implementation.md)

## 验收和代码预算

```sh
make check-teaching STAGE=6
make budget
```

`STAGE=1..6` 依次覆盖无盘双核启动、抢占和栈回收、跨核传输、文件服务、目录复制、替换与迁移。驱动先运行宿主协议测试，再启动验收镜像，最后冷启正常镜像并通过真实串口驱动 shell。`make budget` 统计所有自有运行时 Rust、汇编和链接脚本的物理行，包括注释与空行，用于跟踪数千行的教学规模，行数不作为构建失败条件。测试、构建工具、文档、上游依赖分别放置。

原 Comix 的 Linux ABI、ext4、网络及 LoongArch 版本可从 [基线提交 e2dc94b](https://github.com/comix-kernel/comix/tree/e2dc94bcb05d85e5748a2717af8d87714195eae1) 检出。
