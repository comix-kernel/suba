# Comix 教学内核的结构与运行规约

在 shell 中打开 `/out/cons`，写入四个字节。任务的命名空间决定这条路径通向哪个服务；该服务可以持有另一个服务的文件句柄，把写入转给串口。两个任务可以在不同核上运行，各自在等待请求或回复时交还 CPU。管理员还可以在这个文件保持打开期间发布新版服务。要解释四个字节最终去了哪里，需要知道四件事：路径怎样绑定、谁拥有连接、谁被调度、谁有权操作设备。

当前教学镜像可在仓库根执行 `make run`。实际接口与资源上限见仓库 README。本章保留设计模型，便于与实现对读。基线 `e2dc94b` 仍通过磁盘装载用户程序。下文的启动和接入代码是目标模块的接口片段。标有断言的 Rust 示例可用宿主 rustdoc 执行，覆盖任务等待、请求关闭、文件语义和目录发布。[架构依据与现有源码对照](teaching_kernel_design.md)

## 架构对照与实现

| 机制 | 初版的简化 | 当前运行路径和观察方法 |
| --- | --- | --- |
| 内存与启动 | 固定数组提供页表，栈和堆直接使用链接地址对应的物理内存 | 启动核从 DTB 建页帧位图，扣除固件、镜像、DTB、保留区，分配下级页表和栈堆的物理帧。栈保留无映射保护页。阶段 1 逐级读取最终页表检查权限、分配再归还物理帧；运行中读 `/svc/memory` |
| 调度与等待 | 每核调度和每次通知都扫描所有槽 | 每核 FIFO、预分配唤醒收件箱和每事件等待位图。通知携带任务代号和等待序号，只有所属核从收件箱入队。条件检查后以事件序号提交阻塞；可同时等待队列与时钟，避免有挂起设备 I/O 时忽略新请求。阶段 2、3 覆盖抢占、槽位复用、提前通知和已登记尚未切栈的通知 |
| 文件与命名空间 | 固定路径共享一份字节，挂载表整体复制 | 命名空间按写复制。用 `on 0 mount /dev record; put /dev/cons child` 在子任务内改挂。客户端支持显式偏移和 Create；服务端在打开目录时生成列表快照。内存服务有多个独立文件，两种索引实现通过同一文件协议访问 |
| 设备与通信 | 服务循环遇到 WouldBlock 后按 tick 重试 | 核 0 时钟按有界批次收取 UART 字节到 RX 环。串口服务通过 `FileTree::defer` 取得已打开对象和一次性回复权，可以先处理写、稍后完成读。连接关闭与执行者完结分别计数，迟到回复返回 Closed。阶段 4 在读任务已阻塞时完成另一文件写入，驱动看到确认后才注入串口输入 |
| 组合与生命周期 | 开机固定计数器下游，管理文件只切换一个 store | 每个可替换名称有管理服务。候选先在目标核执行 start；控制 fid 绑定预期版本。服务可撤销、重发、并存或静止迁移。计数器显式接收新下游句柄并导入计数；旧实例排空前在 status 中保留身份。内存快照保存整个目录、内容和修订号，先校验再安装，并能导入初版单文件格式 |
| 跨核一致性 | 日志同时保留完成句柄，一个可见性用例手工插入记录 | 共享日志只含值类型变更与来源核。合并者持有本批回复句柄，公布 tail 后即可唤醒另一核。阶段 5 从正式提交队列送入变更，在追加与本地应用之间设置测试屏障，核对另一核可提前观察以及随后读不倒退；慢核测试保留环形槽直到游标推进 |
| 诊断 | 只有启动日志和自测通过标记 | trace 特性使用每核固定环。park/wake 记录槽号、任务代号、等待序号；queue/deliver/reply 记录实例、连接、tag；append/applied 记录来源核、日志位置和批量，publish 记录名称、实例和代号。通过管理文件读快照；panic 通过 SBI 调试口尽力输出最近事件 |

教学配置继续使用两个 hart、共享根页表、固定栈槽和镜像内预链接实现。页帧池覆盖 QEMU virt 的早期高地址映射窗口；文件服务提供本机类型化协议和易失数据。外部 crate 动态装载、完整网络 9P、磁盘文件系统与运行中故障 hart 恢复属于设计稿列出的扩展。

## 一张页表中的模块和任务

固件在 M 态运行。Comix 的运行时、服务和 shell 链接进一个镜像，用同一张 Sv39 页表在 S 态执行。**模块**决定哪些代码能构造和修改一种资源，**任务**决定哪段代码此刻占用 CPU。一份服务实现可以构造多个实例，每个实例有自己的任务、接收端、文件标识表和业务状态。

```text
核 1：shell 的栈             核 1：计数服务的栈             核 0：串口服务的栈
  /out/cons                     导出 cons、count                导出 cons
     │ 写入打开的文件               │ 写入下游句柄                   │ try_write
     └──── 请求队列 / 回复 ───────→└──── 请求队列 / 回复 ────────→ UART
              队列满、无请求或等待回复时，由所属核的调度器切换任务

打开文件时另走一条路径
  私有挂载表 → 服务名 → 本核目录副本 → 实例管理器 → 固定到该实例的连接
                           ↑
               两个副本按序应用发布日志
```

| 模块与拟议位置 | 拥有的状态 | 对外规约 |
| --- | --- | --- |
| 平台与内存 `os/src/{platform,mm}/` | 页表页、物理帧、预映射栈槽、每核控制块和 UART 寄存器映射 | 分配栈槽、设置时钟、发送 IPI；启动时只构造一个 UART 令牌 |
| 任务运行时 `os/src/runtime/` | 每核当前任务、就绪队列、任务栈和保存的寄存器 | 创建、抢占、阻塞、唤醒、退出；只有所属核能把任务放入自己的就绪队列 |
| 本机传输 `os/src/transport/` | 有界请求队列、等待登记、应答槽 | 移交请求负载，向匹配的调用者交付一次终态；阻塞和跨核唤醒交给运行时 |
| 文件接口 `os/crates/service_api/` | 协议类型；客户端的挂载视图、文件句柄和偏移 | 按路径建立连接，按 fid 操作文件；服务端接收请求，可延迟回复 |
| 实例管理 `os/src/services/` | 候选实例、端点、接入状态、连接与在途计数 | 按名称授权建连，启动、替换及回收实例。向普通组件分发受限的接入句柄 |
| 复制目录 `os/crates/registry/` | 发布日志、各核目录和重放游标 | 对名称做条件更新，查询返回实例 ID 与代号。日志记录只含可以重放的值 |
| 服务与应用 `os/components/` | 内存文件、串口待处理请求、计数、shell 行缓冲 | 依赖安全文件接口，接收构造时授予的资源；组装入口选择具体实现 |

构建依赖由接口决定。组件只链接 `service_api`；镜像组装层把实现该接口的端点、接入句柄或设备能力传入组件。`transport` 使用运行时等待原语，运行时只依赖平台、内存和短临界区。`services` 同时使用传输与目录，才能把可复制的身份转换为真实连接。`registry` 的顺序更新函数可以脱离调度器执行。组件 crate 启用 `#![forbid(unsafe_code)]`；CSR、MMIO、页表和切栈的安全前提由可信底层维护。同一特权级意味着底层越界访问可能损坏整台机器。

## 从启动入口到第一个文件请求

主核从 OpenSBI 接收 hart ID 和 DTB 的物理地址。入口汇编启用临时映射、跳到高地址并切到启动栈。Rust 平台层保留固件、内核和 DTB 占用的内存，建立最终页表，给代码、只读数据和栈设置相应权限，栈槽之间保留保护页。主核发布页表根和每核资源，经 SBI HSM 启动从核。两个核分别设置本核 `tp`、`satp`、`stvec` 和时钟，确认在线后进入各自的调度循环。共享根页表使后续任务切换只需保存执行现场。[现有 RV64 入口](https://github.com/comix-kernel/comix/blob/e2dc94bcb05d85e5748a2717af8d87714195eae1/os/src/arch/riscv/boot/entry.S)

第一个可运行任务是管理任务。以下片段展示输出链的组装。设备令牌移进串口实例，管理任务通过文件协议打开它，再把打开的句柄移进计数实例。每次 `publish_ready` 在候选通过契约检查并发出就绪通知后才发布。完整清单还按同样方式发布 `store` 和每核任务状态服务。

```rust,ignore
fn start_output(manager: &ServiceManager, uart: Uart) -> Result<(), BootError> {
    let console = manager.start(Hart::H0, move || Console::new(uart))?;
    manager.publish_ready("console", console)?;

    let devices = Namespace::empty().mount("/dev", "console")?;
    let sink = devices.open(&manager.connector_for(&["console"]), "/dev/cons", Mode::Write)?;
    let counter = manager.start(Hart::H1, move || Counter::new(sink))?;
    manager.publish_ready("counter", counter)?;

    let view = devices.mount("/out", "counter")?;
    let access = manager.connector_for(&["console", "counter"]);
    manager.spawn(Hart::H1, move || shell_main(view, access))?;
    Ok(())
}
```

`mount` 返回新的挂载视图。持有视图的两个任务可以给 `/dev` 挂不同的服务；即使知道某个服务名，调用者也需要持有允许接入该名称的 `Connector`。计数实例只得到一个可写句柄。管理器也可以把内存后端的可写句柄交给该计数实例。shell 的写入代码保持相同，输出目的地由组装时移入的句柄决定。

### CPU 如何离开一个正在等待的任务

每个任务固定到一个 hart，有独立的栈和上下文。首次入队时 `sp` 指向对齐的栈顶，`ra` 指向取走一次性入口闭包的 `task_start`。平时的切换汇编保存 `ra/sp/s0–s11`；时钟在任意指令处到来，陷入入口还要将完整整数现场、`sepc` 和 `sstatus` 压入被打断任务的栈。任务恢复后沿原栈执行 `sret`。时钟处理设置待调度标记，在允许抢占的返回点换栈。入口闭包返回时，调度器在自己的栈上回收已退出任务的栈槽。[现有切换汇编](https://github.com/comix-kernel/comix/blob/e2dc94bcb05d85e5748a2717af8d87714195eae1/os/src/arch/riscv/kernel/switch.S)

下面的结构把任务栈、寄存器和一次性入口放在同一个所有者下。`Context` 的字段顺序与 RV64 切换汇编一致；`HartLocal` 中的调度器现场允许任务退出时先切离将要释放的栈。

```rust,ignore
#[repr(C)]
struct Context {
    ra: usize,          // switch 恢复后 ret 的目标
    sp: usize,
    saved: [usize; 12], // s0 到 s11
}

struct Task {
    id: TaskId,         // 槽号和代号
    owner: HartId,
    stack: StackSlot,
    context: Context,
    entry: Option<Box<dyn FnOnce() + Send + 'static>>,
    state: TaskState,
    wait: Wait,
}

struct HartLocal {
    scheduler: Context,
    current: Option<TaskId>,
    ready: ReadyQueue<TaskId>,
}
```

阻塞中最容易出错的是消息已经到达，接收者却刚准备睡下。下面的状态机位于**所属核**，描述一次等待从登记到恢复的过程。`Armed` 表示已经登记但仍在 CPU 上，`Notified` 表示通知早于切栈。只有从已阻塞态收到匹配通知时，调度器才需要新添一个就绪队列项。

```rust
#[derive(Debug, PartialEq)]
enum Wait { Idle, Armed(u64), Notified(u64), Blocked(u64) }

impl Wait {
    fn notify(&mut self, seq: u64) -> bool {
        match *self {
            Self::Armed(n) if n == seq => {
                *self = Self::Notified(n);
                false
            }
            Self::Blocked(n) if n == seq => {
                *self = Self::Idle;
                true
            }
            _ => false,
        }
    }

    // 调用者已屏蔽本核中断。true 表示在本次调度交接中切走。
    fn commit_park(&mut self, seq: u64) -> bool {
        match *self {
            Self::Armed(n) if n == seq => {
                *self = Self::Blocked(n);
                true
            }
            Self::Notified(n) if n == seq => {
                *self = Self::Idle;
                false
            }
            _ => unreachable!("等待登记与提交必须配对"),
        }
    }
}

let mut wait = Wait::Armed(7);
assert!(!wait.notify(7));
assert!(!wait.commit_park(7));       // 消费早到通知，继续运行
wait = Wait::Armed(8);
assert!(wait.commit_park(8));        // 已切走
assert!(!wait.notify(7));            // 上一次等待的迟到通知
assert!(wait.notify(8));             // 所属核将任务放回就绪队列
assert!(!wait.notify(8));
```

传输层持队列锁检查条件，在同一临界区登记 `(TaskId, seq)` 并设置 `Armed`。若条件已经满足，直接取消息。发送方在锁内移动负载、摘下等待项，锁外把通知投递到所属核，再按需发 IPI。所属核先核对任务槽的代号，再执行 `notify`。`commit_park` 与真正切离当前栈构成一次屏蔽本核中断的调度交接，跨核发起者始终只写通知。恢复的任务重新检查队列条件。这个边界覆盖了旧消息队列中“释放状态锁以后才登记睡眠”的窗口。[现有消息接收路径](https://github.com/comix-kernel/comix/blob/e2dc94bcb05d85e5748a2717af8d87714195eae1/os/src/ipc/message.rs#L98)

### 两颗核交接一次唤醒

以核 0 上的串口服务等待下一个请求为例。下表中的 `seq=42` 属于这次等待。门铃 IPI 可以合并，通知内容由所属核的收件箱保存。调度器只在任务上下文交接完成后恢复本核中断。

| 时刻 | 核 0 · 接收者 | 核 1 · 发送者 | 必须成立的条件 |
| --- | --- | --- | --- |
| 登记 | 暂停本核抢占，持队列锁发现队列为空，保存等待项并置 `Armed(42)`，随后释放队列锁 | 入队操作也必须取得同一把锁 | 检查为空到登记完成之间，发送者无法插入消息 |
| 交付 | 可以处理中断，暂缓时间片切换直到本次 park 交接结束 | 在锁内入队并取走等待项；解锁后向核 0 的收件箱发布通知，再发 IPI | 接收者见到通知时可以检查到已经入队的请求 |
| 抢在切走前唤醒 | IPI 处理将匹配的 `Armed` 改为 `Notified`；`commit_park` 消费标记，返回原任务继续查队列 | 已经交出负载 | 当前任务仍占用 CPU，普通就绪队列里没有它 |
| 或者先切走 | 关本核中断，将任务置为 `Blocked(42)`，保存现场并切到调度器栈；调度器清除 current 后恢复中断 | 通知可在这段时间进入收件箱 | 所属核下一次处理通知时，只把已离开 CPU 的任务加入就绪队列 |

表的最后两行是同一通知的两种到达次序。若请求先于登记入队，接收者在第一行直接取走负载。复用任务槽时更新任务代号，每次登记分配新的等待序号；二者均在耗尽前停止复用。这样，新任务和老任务的下一次等待都不会接受旧通知。

**收件箱的内存和锁。** 教学实现可为每个任务槽预留一项待唤醒记录。事件源在所属核的收件箱短锁内合并同一身份的通知，保留较新的任务代号和等待序号，随后发送 IPI。所属核在同一锁内取走记录，锁外核对当前任务身份并更新就绪队列。清除本次 IPI 后再排空收件箱，使其间到达的新通知有机会再次触发处理。预留槽也让满请求队列中的关闭和唤醒仍有投递空间。中断路径不获取服务接入锁、不等待队列容量；跨栈切换前释放队列和收件箱守卫。

## 文件协议与传输规约

三个标识各有寿命。`ConnectionId` 标识一次接入并固定一个实例；`Fid` 标识该连接内一次遍历或打开的对象；`Tag` 标识一个等待回复的请求。同一连接可以同时有多个请求，回复顺序由服务决定。以下为协议骨架，传输层签发连接号和应答权，服务端按 `(connection, fid)` 查自己的表。

```rust,ignore
enum FileOp {
    Attach { root: Fid },
    Walk   { from: Fid, to: Fid, names: Box<[Name]> },
    Open   { fid: Fid, mode: Mode },
    Read   { fid: Fid, offset: u64, len: usize },
    Write  { fid: Fid, offset: u64, bytes: Box<[u8]> },
    Clunk  { fid: Fid },
}

struct Request { connection: ConnectionId, tag: Tag, op: FileOp }
enum Response { Done, Data(Box<[u8]>), Written(usize) }

trait FileService {
    fn handle(&mut self, request: Request, reply: Reply);
}

impl Reply {
    fn send(self, result: Result<Response, FileError>) -> Result<(), Closed> {
        self.slot.complete_once(result)
    }
}
```

| 操作 | 成功后的状态 | 失败或关闭时的规约 |
| --- | --- | --- |
| Attach、Walk | 建立根 fid；Walk 从源 fid 遍历，全部成功后安装目标 fid | 重复 fid 或无效分量返回错误，原 fid 仍可使用，客户端清理临时 fid |
| Open、Read、Write | Open 固定访问模式；普通文件按实际传输量推进句柄偏移，串口按流处理 | 服务检查模式和长度。短写返回已确认长度；写入已交付却失去回复时返回 `WriteOutcomeUnknown` |
| Clunk、断连 | Clunk 释放一个 fid；最后一个连接关闭后服务回收余下 fid 和挂起请求 | 析构只登记关闭并通知服务，满队列也能传递关闭信号。等待空间、请求或回复的任务都能结束等待 |

每个实例有一条同时限制消息数和总字节数的队列。发送者先保留应答槽，入队时把缓冲区所有权交给队列；容量不足时按上一节的规约等待。接收端唯一，服务取出请求后在锁外调用 `handle`。一次性 `Reply` 允许 UART 服务缺少输入时保存回复权，继续处理其他请求。丢弃未完成的 Reply 时，析构函数按交付状态结束等待。正常回复、放弃回复和连接关闭竞争同一应答槽的终态；槽与具体请求绑定，迟到回复无法完成复用 tag 的新请求。设备写可能已经生效的错误沿原调用链返回，传输层不会重发。

例如任务同时挂载了 `/dev` 和 `/dev/log`，打开 `/dev/log/today` 由后者处理，`/dev/logger` 则交给前者。匹配单位为完整路径分量。命中较长挂载后若接入权限不足，open 直接报告拒绝；退回较短挂载会让同一绝对路径在权限错误时指向另一对象。

一次 open 也可能只完成一半。客户先以 fid 1 Attach，再 Walk 到临时 fid 2。Walk 失败时服务保留 fid 1，fid 2 未安装；若 Walk 成功而 Open 失败，客户端负责 Clunk 已取得的两个临时 fid。连接已断时由服务按连接清理。Open 成功后客户端释放临时根 fid，把目标 fid 交给 `FileHandle`。同一 fid 关闭前已受理的操作持有对象引用；只有本轮 fid 的请求都已收尾，客户端才复用其编号。这样，同一服务里另一条连接即使也有 fid 2，也不会被本次失败清理影响。

### 在哪一步还能撤销写入

设连接 C 的 tag 12 正在写 UART。即使 C 的最后一个客户端已经退出，可能还有一项工作在设备侧。为此分别记录**调用者的结果**和**执行端是否已释放工作引用**。前者决定何时唤醒 shell，后者决定何时能够回收旧实例。本文选择接收者在队列锁内取走消息并取得执行权的瞬间作为交付边界。

下面是一个写请求的顺序规格。每个方法在保护对应状态的短临界区调用。`finish` 返回本次是否新产生了可通知调用者的结果。实例的工作引用随执行端实际完结而释放，即使关闭方已经先行通知了调用者。

```rust
#[derive(Debug, PartialEq, Eq)]
enum WriteResult { Written(usize), Closed, Unknown }

#[derive(Debug, PartialEq, Eq)]
enum Work { Queued, Executing, Released }

struct WriteCall { work: Work, result: Option<WriteResult> }

impl WriteCall {
    fn queued() -> Self {
        Self { work: Work::Queued, result: None }
    }

    fn take(&mut self) -> bool {
        if self.work != Work::Queued { return false; }
        self.work = Work::Executing;
        true
    }

    fn close(&mut self) {
        if self.result.is_some() { return; }
        self.result = Some(match self.work {
            Work::Queued => {
                self.work = Work::Released;
                WriteResult::Closed
            }
            Work::Executing => WriteResult::Unknown,
            Work::Released => unreachable!("已释放的写请求已有结果"),
        });
    }

    fn finish(&mut self, outcome: WriteResult) -> bool {
        assert_eq!(self.work, Work::Executing);
        self.work = Work::Released;
        if self.result.is_some() { return false; }
        self.result = Some(outcome);
        true
    }
}

let mut cancelled = WriteCall::queued();
cancelled.close();
assert!(!cancelled.take());
assert_eq!(cancelled.result, Some(WriteResult::Closed));

let mut disconnected = WriteCall::queued();
assert!(disconnected.take());
disconnected.close();
assert_eq!(disconnected.work, Work::Executing);
assert!(!disconnected.finish(WriteResult::Written(2)));
assert_eq!(disconnected.result, Some(WriteResult::Unknown));
assert_eq!(disconnected.work, Work::Released);

let mut replied = WriteCall::queued();
assert!(replied.take());
assert!(replied.finish(WriteResult::Written(2)));
replied.close();
assert_eq!(replied.result, Some(WriteResult::Written(2)));
```

实际队列中，take 与取消排队项在**同一把队列锁**下竞争：前者摘下负载并设置 Executing，后者摘下负载、归还条目与字节配额，再结束应答槽。两条路径需要同时碰到槽时均按“队列 → 应答槽”取锁。回复只取槽锁。所有唤醒放在解锁之后。等待容量且尚未入队的发送者被关闭唤醒后直接返回 Closed。

最后一个客户端句柄的析构只置连接关闭位并通知服务。入队和取件都在队列临界区复核关闭位；已经完成取件的请求由执行端收尾。已登记的应答槽和设备工作引用可以继续存活，它们不计入客户端句柄数。服务看到关闭位后拒收该连接的新请求，清除排队项、释放 fid，唤醒受影响等待者；未调用硬件且能确认取消的工作可直接收尾。已经交付又缺少确认的写向调用者报告结果未知，执行端在收到完成通知或确认设备取消后才释放工作引用。普通 `Clunk` 只关闭指定 fid，同一连接的其他 fid 继续使用。已开始的操作持有独立引用，迟到的回复始终指向原应答槽。

### 跟踪一次短写

shell 打开 `/out/cons` 时，客户端按路径分量查最长挂载前缀 `/out`，接入 `counter` 实例，依次发送 Attach、Walk(`cons`) 和 Open。此后的 Write 直接使用这个实例上的 fid。计数服务验过 fid 的模式后调用下面的写处理函数。这里把下游可写句柄的窄接口写成 `ByteSink`；在镜像中由 `FileHandle` 实现，它通过自己的连接发送第二个 Write。

```rust
#[derive(Debug, PartialEq)]
enum WriteError { NoCapacity, WriteOutcomeUnknown }

trait ByteSink {
    // 成功值至多等于交给下游的字节数。
    fn write(&mut self, bytes: Box<[u8]>) -> Result<usize, WriteError>;
}

struct Counter<S> { sink: S, confirmed: usize }

impl<S: ByteSink> Counter<S> {
    fn write(&mut self, bytes: Box<[u8]>) -> Result<usize, WriteError> {
        if bytes.is_empty() { return Ok(0); }
        let room = usize::MAX - self.confirmed;
        if room == 0 { return Err(WriteError::NoCapacity); }
        let mut bytes = bytes.into_vec();
        bytes.truncate(room);
        let n = self.sink.write(bytes.into_boxed_slice())?;
        self.confirmed += n;
        Ok(n)
    }
}

struct ShortWriter;
impl ByteSink for ShortWriter {
    fn write(&mut self, bytes: Box<[u8]>) -> Result<usize, WriteError> {
        Ok(bytes.len().min(2))
    }
}

let mut counter = Counter { sink: ShortWriter, confirmed: 0 };
assert_eq!(counter.write(Box::new(*b"abcd")), Ok(2));
assert_eq!(counter.confirmed, 2);
```

UART 确认两个字节后，串口服务回复其下游请求，计数任务被唤醒，更新 `confirmed`，再用上游的 Reply 向 shell 返回 2。`/out/count` 由同一个服务任务读取这个字段。若 UART 已发出字节但回复丢失，函数中的 `?` 会在累加之前返回；此时计数表示已获确认的字节。文件客户端应在向组件交付 `Written(n)` 前核对 `n <= 请求长度`，使 `ByteSink` 的成功规约成立。组装层将已打开的下游句柄逐层传入，同步转发路径沿既有依赖向下执行。

任务状态文件展示了同一协议如何呈现一个内核对象。每核状态服务持有运行时分发的本核只读查询能力。Walk 解析 `/tasks/0/<id>/status` 的任务身份，Open 在本核短临界区检查槽号和代号并复制调度状态，退出临界区后格式化文本，存入这个 fid。Read 按偏移取该快照。任务在打开前退出时返回 `NotFound`；打开后退出或复用槽位，原读者仍能读完同一次采样。服务始终只持有值，调度器保有修改任务的唯一权力。

### 把内核对象交给文件服务的最小接口

状态服务只需要一次采样。下例中的 `TaskSlot` 由运行时持有，`sample` 在所属核短临界区调用。它返回可复制的值，服务退出临界区后才分配和格式化文本。这样，一个客户端慢慢读文件时不会持有调度器锁。每次成功 Open 把一个 `StatusFile` 移入对应 fid；关闭 fid 即可回收这份文本。

```rust
#[derive(Clone, Copy)]
struct Sample { hart: u16, generation: u64, state: &'static str }

struct TaskSlot { live: Option<Sample> }

impl TaskSlot {
    fn sample(&self, generation: u64) -> Option<Sample> {
        self.live.filter(|task| task.generation == generation)
    }
}

struct StatusFile { text: Box<[u8]> }

impl StatusFile {
    fn open(task: Sample) -> Self {
        let text = format!("hart={} gen={} {}\n", task.hart, task.generation, task.state);
        Self { text: text.into_bytes().into_boxed_slice() }
    }

    fn read(&self, offset: u64, count: usize) -> &[u8] {
        let start = usize::try_from(offset).unwrap_or(usize::MAX).min(self.text.len());
        let len = count.min(self.text.len() - start);
        &self.text[start..start + len]
    }
}

let old = Sample { hart: 0, generation: 7, state: "Blocked" };
let mut slot = TaskSlot { live: Some(old) };
let fid = StatusFile::open(slot.sample(7).unwrap());
let first = fid.read(0, 5).to_vec();
slot.live = Some(Sample { generation: 8, state: "Ready", ..old });
assert!(slot.sample(7).is_none());
assert_eq!([first.as_slice(), fid.read(5, usize::MAX)].concat(), b"hart=0 gen=7 Blocked\n");
assert!(fid.read(u64::MAX, 1).is_empty());
```

运行时在实际采样前还核对申请者的 hart 权限和槽号范围。示例里的切片只在服务处理 Read 的作用域内借用。回复把该段字节移入自己的缓冲区，因此一个已发出的回复也能在 Clunk 之后完成。这里有三个不同的寿命：任务活到退出，快照活到 fid 关闭，回复负载活到客户端接收或连接清理。

## 服务目录与实例替换

每个已知服务名占一个目录槽。先在一份目录上定义其行为：查询返回可选绑定，条件写比较提交者读到的槽版本，然后安装新实例或撤销名称。槽即使为空也保留版本。比如管理器在空槽 0 上准备发布 A，期间 B 被发布又撤销，此时空槽的版本已是 2；早先准备的 A 必须重新核对。这保证名称撤销、再发布以及实例表复用槽号时，过期的接入尝试都可被识别。

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Slot { revision: u64, instance: Option<u32> }

#[derive(Clone, Copy)]
struct Change { expected: Slot, next: Option<u32> }

#[derive(Debug, PartialEq)]
enum PublishError { Conflict, Exhausted }

fn apply(slot: &mut Slot, change: Change) -> Result<(), PublishError> {
    if *slot != change.expected { return Err(PublishError::Conflict); }
    let revision = slot.revision.checked_add(1).ok_or(PublishError::Exhausted)?;
    *slot = Slot { revision, instance: change.next };
    Ok(())
}

let empty = Slot::default();
let publish = Change { expected: empty, next: Some(3) };
let published = Slot { revision: 1, instance: Some(3) };
let remove = Change { expected: published, next: None };
let mut replicas = [empty, empty];
for slot in &mut replicas {
    assert_eq!(apply(slot, publish), Ok(()));
    assert_eq!(apply(slot, remove), Ok(()));
    assert_eq!(apply(slot, publish), Err(PublishError::Conflict));
}
assert_eq!(replicas, [Slot { revision: 2, instance: None }; 2]);

let expected = replicas[0];
for slot in &mut replicas {
    assert_eq!(apply(slot, Change { expected, next: Some(4) }), Ok(()));
    assert_eq!(apply(slot, Change { expected, next: Some(5) }), Err(PublishError::Conflict));
}
assert_eq!(replicas, [Slot { revision: 3, instance: Some(4) }; 2]);
```

对客户端，非空槽投影为 `Binding { instance, generation: revision }`，空槽返回 NotFound。对管理器，准备变更时连空槽的版本也一同保留。目录槽版本每次成功修改只增一次；日志序号则对每次进入日志的操作都递增，包括条件冲突。内存文件的修订号描述内容变更，由文件服务自己保存。这三种计数分别回答“连到谁”“执行到哪里”“读的是哪份数据”。

### 两核如何读到同一个发布结果

共享环形日志的 `tail` 是已完整填入的连续前缀末端，初值为 0。`applied[h]` 是核 h 完成应用的最后一个序号。每核任务合并本核待提交操作，持日志短锁填入有空间的一批，最后以 release 公布新 tail。查询者以 acquire 采样一次 tail，等本核至少应用到该位置，再持本地读保护取值。副本的唯一写者对顺序函数求值后发布游标，只有发起核完成该提交者的应答。复制不获取服务接入锁，日志重放期间也没有调用设备的接口。

下面是环形日志的**顺序模型**。把两个副本的步骤交错执行，可以直接观察“写者何时必须等”和“晚读者最少应追到哪里”。模型每次追加一项，镜像里的批量追加使用同样的空间条件。`&mut self` 在模型中串行化步骤，镜像中追加、读取条目和执行副本更新分属不同临界区。

```rust
struct Ring<T, const N: usize> {
    entries: [Option<T>; N],
    tail: usize,
    applied: [usize; 2],
}

impl<T: Copy, const N: usize> Ring<T, N> {
    fn new() -> Self {
        assert!(N > 0);
        Self { entries: [None; N], tail: 0, applied: [0; 2] }
    }

    fn try_append(&mut self, entry: T) -> bool {
        let oldest = self.applied[0].min(self.applied[1]);
        if self.tail - oldest == N { return false; }
        let next = self.tail.checked_add(1).expect("模型序号耗尽");
        self.entries[self.tail % N] = Some(entry);
        self.tail = next;
        true
    }

    fn catch_up(&mut self, hart: usize, target: usize, mut apply: impl FnMut(T)) {
        assert!(target <= self.tail);
        while self.applied[hart] < target {
            let index = self.applied[hart] % N;
            apply(self.entries[index].expect("已发布的条目"));
            self.applied[hart] += 1;
        }
    }
}

let mut log = Ring::<u8, 2>::new();
let mut history = [Vec::new(), Vec::new()];
assert!(log.try_append(7));
let observed = log.tail;
log.catch_up(0, observed, |x| history[0].push(x));
assert!(log.try_append(8));
assert!(!log.try_append(9));                     // 核 1 仍需第一项
log.catch_up(1, observed, |x| history[1].push(x));
assert_eq!(history[1], [7]);                     // 足以回答采样时的查询
assert!(log.try_append(9));                      // 此时才可覆盖首槽
for (hart, seen) in history.iter_mut().enumerate() {
    log.catch_up(hart, log.tail, |x| seen.push(x));
}
assert_eq!(history, [vec![7, 8, 9], vec![7, 8, 9]]);
for entry in 10..26 {
    assert!(log.try_append(entry));
    for (hart, seen) in history.iter_mut().enumerate() {
        log.catch_up(hart, log.tail, |x| seen.push(x));
    }
}
assert_eq!(history[0], (7..26).collect::<Vec<_>>());
assert_eq!(history[0], history[1]);
```

在镜像里，推进任务从受保护的日志槽复制一条值记录，释放日志锁后再修改本地目录。更新完成才以 release 公布 applied，追加者以 acquire 检查两个游标后复用环形槽。实际序号耗尽返回资源错误。日志暂满时，写者在容量检查的同一临界区登记等待并通知慢核；副本前进后唤醒写者。空闲核也调度推进任务。这样，持有可休眠接入锁并等待发布的管理器不会挡住为它腾出空间的副本。

以 `console` 从 A/7 换到 B/8 为例。日志第 31 项已经完整发布，但管理器可能还没收到应答。此刻核 0 若已重放到 31，可以查到 B/8。之后才开始的核 1 查询，采样到的 tail 至少为 31，即使本核游标还在 29，它也要应用第 30、31 项才返回。若中间又发生替换，允许读到更新的版本。此时尚有一个单独的问题：看到 B/8 的调用者什么时候可以取得 B 的端点？

**从绑定到实际连接。** 目录查询和取得实例引用之间可以发生切换。下面是连接器的核心调用路径。`admit_if_current` 在同一把名称接入锁内检查“当前接入绑定等于查询值、实例处于 Active”，并取得连接租约。目录更新者也持有这把锁，直到新版开放接入。因而读者即使提前看见新目录值，也只能在切换完成后拿到对应端点。

```rust,ignore
fn connect(&self, name: &ServiceName) -> Result<Connection, ConnectError> {
    self.grant.check(name)?;
    loop {
        let seen = self.registry.lookup(name).ok_or(ConnectError::NotFound)?;
        match self.instances.admit_if_current(name, seen) {
            Admission::Granted(lease) => return Connection::from_lease(lease),
            Admission::Replaced => continue, // 尚未发送文件请求，可以重查
            Admission::Migrating => return Err(ConnectError::Busy),
        }
    }
}
```

| 实例状态 | 新连接 | 已有工作与转换条件 |
| --- | --- | --- |
| Starting | 由管理器私有持有 | 新任务建立接收端，检查服务契约，必要时导入快照，发出就绪通知 |
| Active | 接入锁内取得实例租约 | 每个连接固定端点和发布代号，已打开的 fid 始终归这个实例 |
| Draining | 新调用按名称查新版 | 旧连接和已交付请求在旧实例完成，设备回调仍持有旧实例的工作引用 |
| Retired | 实例已移出接入表 | 连接、在途请求及回调全部清空，旧任务停止并交回执行权，管理器释放端点与业务状态 |

### 管理器何时可以销毁旧实例

假设核 1 的 shell 已经取得 A/7 的连接，核 0 的管理器要把同名服务切到 B/8。目录提交之前，B 的任务、端点、快照空间和实例表项都要准备好。提交之后，持接入锁的管理器只切换已准备好的状态。连接租约的析构释放连接计数；请求、设备回调和任务退出各有独立的完成通知。简单地检查某个 `Arc` 的强引用数无法区分这些条件。

| 竞争时刻 | 对客户端的承诺 | 可以释放的资源 |
| --- | --- | --- |
| 核 1 查到 A/7，尚未取得租约；核 0 已切换 | 核 1 在接入锁下发现版本过期，释放锁并重查，此时还没有发送 Attach | A 停止新接入；已有租约、fid 和请求继续归 A |
| 核 1 已持 A 的租约；B/8 已开放 | 新打开的同名文件取得 B。旧句柄仍能向 A 发请求直至关闭 | 只要旧租约还在，A 的任务和端点就保留 |
| shell 关闭最后的 A 连接，但 A 有一笔已交付写 | 应答槽按写规约结束等待。设备完成事件仍能找到 A，迟到回复只触及原槽 | 已取消的队列项和已关闭的 fid 可回收，设备工作引用继续持有 A |
| A 最后一笔工作完结 | 管理器通知 A 停止并等待退出确认。运行时切离 A 的栈后归还栈槽 | 管理器摘除 A 的实例表项并释放端点；预链接的机器码仍在镜像中 |

**带状态迁移还有一个可失败的窗口。** 管理器先在名称锁内为旧绑定取得本次迁移的所有权并关闭接入。如果连接、工作或回调仍被占用，当场恢复接入并返回 Busy。旧实例静止后，管理器释放名称锁，经管理通道导出快照、在 B 上校验和导入。取消或导入失败时，重新持锁核对旧绑定和迁移身份，只清除自己的标记，再销毁 B。成功时在锁内条件提交并开放 B，随后停止 A。此流程允许另一个服务名正常接入，也保证一次过期的失败清理不会误开已经退役的 A。
