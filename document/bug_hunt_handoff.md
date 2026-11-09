# 缺陷排查交接

日期：2026-11-04。本文只记进度、核对约定和还没读完的范围。缺陷条目只维护在 [known_bugs.md](known_bugs.md)。不要在各子系统设计文档里再开一份清单。

核对方式是顺着调用链读当前源码，并和 POSIX / Linux 语义比较。这些条目没有在 QEMU 里逐条复现。内核代码没有因为这份清单而修改。未实现、但会返回明确错误码的接口不记入清单。

## 本轮写入清单的标题

网络 14 条，加在「网络」一节末尾：

- 关闭监听 socket 时，尚未 accept 的连接被直接摘掉
- `socketpair` 的 UNIX 数据报 `sendto` 忽略目的地址
- 流式 `sendto` 不区分是否已经连接
- `recvfrom` 把 `addrlen` 写成实际拷贝的字节数
- 套接字拷贝失败仍返回成功，接收队列已经被消费
- `MSG_PEEK` 会把数据从队列里删掉
- TCP `connect` 把正在连接当成已经连接，并覆盖 `bind` 的地址
- UDP 的 `connect` 和 `sendto` 改写已经绑定的本地地址
- 两个 TCP socket 可以 `listen` 同一个端口
- UNIX 数据报 `connect` 不查找对端，发给流式 socket 的数据报没人读
- UNIX 路径名和绑定表各记一份
- `getpeername` 把没有记下的对端当成未连接
- UNIX 数据报的可写判断不看队列
- 只有首字节为 127 的帧才会环回

LoongArch 4 条，单独成节：

- `clone` 的第 4、5 个参数装反了
- 用户态异常把整个内核 panic 掉
- 用 `ESTAT.IS` 判断这次陷阱是不是中断
- 信号帧里的通用寄存器错一格

在此之前，清单里已经有进程与信号、内存、路径和 VFS、用户内存拷贝、网络、调度与时钟、陷阱和程序加载、挂载与 proc、块设备与控制台、终端和进程组、共享内存与映射。继续排查时先读 `known_bugs.md` 的标题，已有标题不要再报。

## 追加条目时的约定

每条保持五段：严重度、位置、现象、触发、修复。严重度只有致命、高、中。位置用仓库根相对路径。现象写调用链上已经读到的行为。触发写成用户程序能碰到的调用。修复写应有的结果，不要改源码，也不要把旧条目改成「已修复」，除非对应代码已经改掉并注明提交。

子代理只做只读排查，不改文件，不写文档。类型用通用代理，不要指定 Bevy 或调试专用类型。收回来的说法先按它给出的文件和函数再读一遍，对上了再追加。

描述 virtio-net、virtio-blk 时停在内核调用点。`virtio-drivers` 0.12 的源码不在本地仓库里，不要替它编内部函数的行为。smoltcp 0.12 的 `is_open` 已对照文档：状态不是 `CLOSED` 和 `TIME-WAIT` 时为真。`listen` 的失败只有「这个 socket 自己已经打开」和「端口为 0」，内核再把错误映射成 `AddressInUse`。除此以外不要发明协议栈内部状态机。

## 读过但不要记成缺陷的结论

- `setuid` / `setgid` 对非零 ID 返回 `EPERM` 是单 root 设计。凭证始终是 root。
- 普通 `connect` 过的 UNIX 数据报，`sendto` 会按传入地址查找。忽略目的地址的只有设置了 `dgram_peer` 的 `socketpair`。
- 本地地址为空时，`getsockname` 写地址族和长度 2。错的是 `getpeername` 在同样情况下返回 `ENOTCONN`。
- `madvise` 的 advice 本身是有意的空操作。清单里只记了未映射区间仍返回成功。
- `setsid` 在调用者已经是进程组首领时失败，符合 POSIX。已记录的问题是它只改当前线程，以及 `setpgid` 用 tid 判断会话首领。
- `Task::Drop` 只打日志。`clone` 失败后多出来的共享内存 `nattch` 因此没人减，这一条已经在共享内存一节。
- 消息队列在放下状态锁之后才睡眠，但没有 `msgsnd` / `msgrcv`。System V 信号量和 `msync` 没有系统调用入口。只留在「尚未覆盖」，不要写成用户能触发的条目。
- LoongArch 的加载器、auxv 常量和页权限与 RISC-V 是同一套，没有另写专有条目。信号帧空洞的字节数，musl 的 LoongArch 头文件这次没有核对；寄存器错位已经按 Linux `sigcontext` 写入清单，不要把两件事并成一个新结论。

## 下一轮还没读完的范围

上一轮派出的四路阅读在会话中断时被取消，没有报告：

- tmpfs，以及页缓存的写回和文件映射的脏页
- ext4 的写路径、目录项、硬链接和 `rename`。删除名字时无条件释放 inode、读失败返回全零、短页被当成文件结束，这三条已经在清单里
- `exec` 的参数、环境和 shebang。ELF 装载权限、重叠页、解释器偏移、未知重定位、`AT_RANDOM` / `AT_EXECFN` / `AT_HWCAP` 已经在清单里
- 上面三条以外的 futex。丢失唤醒、`FUTEX_WAIT_BITSET` 的超时、`FUTEX_REQUEUE` 之后仍查旧地址，不要再报
- 凭证和能力检查。跳过上面的 `setuid` / `setgid`
- virtio-gpu、virtio-input、virtio-console、IPI、工作队列。virtio-blk 的睡眠锁、virtio-net 的接收缓冲、串口回显、Goldfish RTC、RamDisk 回绕、调度里的双跑，都已经记录

另外两处还开着：LoongArch 的 TLB refill 入口，以及 `clone` 以外还有没有架构相关的系统调用参数顺序；消息队列在出现系统调用入口之前保持现在的注记。
