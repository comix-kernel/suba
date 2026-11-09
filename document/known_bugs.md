# 已知缺陷

本文记录已经对照当前源码核对过的逻辑错误。核对方式是顺着调用链读实现，并和 POSIX / Linux riscv64 语义比较。这些条目还没有在 QEMU 里逐条复现。未实现、但会返回明确错误码的接口不记在这里。

日期：2026-11-04。后续新发现追加到文末对应小节，不要把已有条目改写成“已修复”，除非对应代码已经改掉并注明提交。排查进度和下一轮范围在 [bug_hunt_handoff.md](bug_hunt_handoff.md)。新的缺陷仍只追加在本文。

2026-11-04 复核：对交接文档中新追加的网络 14 条和 LoongArch 4 条再次核对调用链及 ABI。17 条保留核心缺陷，错误的描述、触发条件和修复建议已在对应条目纠正；原「UDP 的 `connect` 和 `sendto` 改写已经绑定的本地地址」移至文末「复核后撤回的条目」，不再计入已确认缺陷。网络语义另用宿主机 Linux 最小用例交叉验证，UDP 另做抽取源码函数的宿主机隔离验证；这些都不代表 Comix QEMU 复现，内核代码未修改。

严重度按用户程序会碰到的后果划分：

- 致命：进程停死后无法继续，或内核页表指向已释放的物理页。
- 高：系统调用结果违反 ABI，或会写错、删错、丢数据。
- 中：边界或特定 flag 下出错，主路径仍可能跑通。

## 进程、信号与同步

### fork 丢掉信号处理

- 严重度：高
- 位置：`os/src/kernel/syscall/task/clone_ops.rs`（无 `CLONE_SIGHAND` 分支）
- 现象：普通 `fork` 只带 `SIGCHLD`，子进程拿到一张空的 `SignalHandlerTable`，全部变成 `SIG_DFL`。父进程设置的 `SIG_IGN` 和自定义处理函数都不会留下。未决信号清空、屏蔽字按值复制是对的。
- 触发：父进程忽略 `SIGPIPE` 或装上 `SIGINT` 处理函数后再 `fork`。
- 修复：非 `CLONE_SIGHAND` 时深拷贝动作表。有 `CLONE_SIGHAND` 时也不要共享 `sigaltstack`，备用信号栈是每个线程自己的。

### execve 不恢复已捕获信号，并先改共享 FD 表

- 严重度：高
- 位置：`os/src/kernel/task/task_struct.rs` 的 `Task::execve`；`os/src/kernel/syscall/task/exec_ops.rs` 的 `do_execve_switch`
- 现象：地址空间换掉之后，`signal_handlers` 原样保留。已捕获处理函数的地址属于旧程序。`do_execve_switch` 还先对当前 `fd_table` 调用 `close_exec()`。线程通过 `CLONE_FILES` 共享这张表，带 `FD_CLOEXEC` 的描述符在取消共享之前就被关掉。同路径也没有停掉同组其他线程。
- 触发：`sigaction` 之后在同一进程里 `execve`；或多线程进程执行带 `FD_CLOEXEC` 的 `execve`。
- 修复：先停掉其他线程；在私有 FD 表上关闭 `FD_CLOEXEC`；处理函数不是 `SIG_IGN` 的项重置为 `SIG_DFL`。

### wait4 把正在运行当成 WCONTINUED

- 严重度：高
- 位置：`os/src/kernel/syscall/task/wait_ops.rs`
- 现象：带 `WCONTINUED` 时，任何 `Running` 子进程都立即匹配，并写成 `WIFCONTINUED`。`check_child` 只从 `children` 里摘掉僵尸，所以同一次继续状态会被反复报告。带了 `WUNTRACED` 或 `WCONTINUED` 之后，`check_exited` 变为假。Linux 的 `wait4` 会在内部补上 `WEXITED`。`children` 按顺序取第一个命中项，前面有一个仍在运行或已停止的子进程时，后面的僵尸不会被回收。停止状态是电平触发，不是一次性事件。
- 触发：shell 常用的 `waitpid(-1, &st, WUNTRACED | WCONTINUED)`。
- 修复：停止和继续都记一次性事件，报告后清除。这两种 flag 存在时仍然要能回收已退出的子进程。

### 子进程挂在调用者的 children 上，退出通知按 pid 当 tid 查

- 严重度：高
- 位置：`os/src/kernel/syscall/task/clone_ops.rs`；`os/src/kernel/syscall/task/exit_ops.rs`；`os/src/kernel/task/mod.rs` 的 `notify_parent`；`os/src/kernel/task/task_manager.rs` 的 `get_task`
- 现象：`clone` 一律把新任务推进当前任务的 `children`，包括 `CLONE_THREAD`。`ppid` 却写成进程 pid。`get_task` 的键是 tid。非 leader 线程 `fork` 时，子进程挂在该线程下面，`SIGCHLD` 和 `wait_child` 唤醒发给 leader。leader 的 `wait` 扫不到这个孩子，fork 线程上阻塞的 `wait` 也不会被叫醒。非 leader 的 `exit` 不调用 `notify_parent`，但线程僵尸仍留在创建者的 `children` 里，之后的 `wait` 会把它当成子进程收掉，返回值用的是 tid。
- 触发：主线程 `pthread_create` 之后 `waitpid(-1)`；或非 leader 线程 `fork` 再由 leader `wait`。
- 修复：`CLONE_THREAD` 不要进入 `children`。子进程挂到真正的父进程（线程组则挂到 leader）。`wait` 返回 pid，并按 `exit_signal` 过滤。

### SIGSTOP 之后无法继续，也无法被 SIGKILL

- 严重度：致命
- 位置：`os/src/ipc/signal.rs` 的 `sig_stop` / `sig_continue`；`os/src/kernel/scheduler/rr_scheduler.rs` 的 `sleep_task`；`os/src/kernel/scheduler/mod.rs` 的 `wake_up_task`
- 现象：`sig_stop` 先写成 `Stopped`，随后 `sleep_task(task, false)` 覆盖成 `Uninterruptible` 并移出运行队列。`sig_continue` 只认 `Stopped`。`send_signal` 只唤醒 `Interruptible`。任务不在运行队列上，也走不到 `check_signal`，所以 `SIGCONT` 和 `SIGKILL` 都送不进去。`sig_continue` 还有第二层错误：它先把状态改成 `Running`，`wake_up_task` 看到 `Running` 就返回，不会重新入队。
- 触发：默认处置下收到 `SIGSTOP`、`SIGTSTP`、`SIGTTIN` 或 `SIGTTOU`。
- 修复：停止保持 `Stopped` 并出队。`SIGCONT` 和 `SIGKILL` 必须把这种任务重新放回运行队列。入队前不要先把状态改成 `Running`。

### rt_sigsuspend 睡死，或提前恢复掩码

- 严重度：高
- 位置：`os/src/kernel/syscall/signal.rs` 的 `rt_sigsuspend`
- 现象：换上新掩码的准备函数固定返回“需要睡眠”，不看新掩码下是否已经有可投递信号。信号若在调用前就 pending，没有新的唤醒，调用不返回。被新信号叫醒之后，函数先写回旧掩码再从系统调用返回。信号处理发生在返回之前，临时掩码已经失效，这次放开的信号又被挡住。
- 触发：`SIGUSR1` 被屏蔽且已经 pending，然后 `sigsuspend` 一套不屏蔽它的掩码。
- 修复：新掩码下已有可投递信号就不要睡。旧掩码等处理函数经 `rt_sigreturn` 回来后再恢复。

### rt_sigtimedwait 把等待集合当成屏蔽字

- 严重度：高
- 位置：`os/src/kernel/syscall/signal.rs` 的 `wait_for_signal`；`os/src/ipc/signal.rs` 的 `has_deliverable_signal`
- 现象：零超时和带超时路径把等待集合传给 `has_deliverable_signal`。该函数把参数当作屏蔽字，判断式是 `pending & !mask`。想等的信号被当成已屏蔽；不在集合里的 pending 信号会被摘掉并当作成功返回。无限等待路径用的是交集，方向是对的。超时入队用的是 `timeout.into_freq()`，这是相对 tick，而 `TIMER_QUEUE` 存的是绝对到期时刻。
- 触发：只 pending `SIGUSR1` 时，`rt_sigtimedwait({SIGUSR1}, …, {0,0})` 返回 `EAGAIN`。同时 pending `SIGUSR2` 时，调用会吃掉 `SIGUSR2`。
- 修复：与无限等待一样取目标集合的交集，并再去掉线程已经屏蔽的信号。定时器写入 `get_time() + 相对 tick`。

### futex 丢失唤醒

- 严重度：高
- 位置：`os/src/kernel/syscall/task/futex_ops.rs` 的 `futex_wait_common`
- 现象：用户空间的值在入队之前读。入队之后的准备函数只检查信号，不再读 futex 字。另一侧可以在这两步之间改掉该字并 `FUTEX_WAKE`；当时队列是空的，唤醒计数为 0。等待者随后睡下。
- 触发：互斥锁或条件变量发生争用。
- 修复：入队之后重新读用户空间的值，不一致就出队并返回 `EAGAIN`。

### FUTEX_WAIT_BITSET 把相对超时当成绝对时间

- 严重度：中
- 位置：`os/src/kernel/syscall/task/futex_ops.rs`
- 现象：`FUTEX_WAIT_BITSET` 固定走绝对超时。没有 `FUTEX_CLOCK_REALTIME` 时，Linux 把这个超时解释为相对时间。相对的 1 秒会被当成早已过去的绝对时刻，立即返回 `ETIMEDOUT`。
- 修复：只有带 `FUTEX_CLOCK_REALTIME` 时才按绝对时间解释。

### FUTEX_REQUEUE 之后超时仍查旧地址

- 严重度：高
- 位置：`os/src/kernel/syscall/task/futex_ops.rs`；`os/src/arch/riscv/trap/trap_handler.rs` 的时钟到期唤醒
- 现象：等待者被挪到第二个 futex 字上之后，醒来仍只查原来的物理地址。时钟中断只 `wake_up_task`，不从 futex 队列摘人。超时发生时函数返回 0，节点还挂在新地址的队列上。下一次对该地址的 `FUTEX_WAKE` 会先弹出这个已经回到用户态的任务。
- 触发：带超时的 `FUTEX_WAIT`，随后 `FUTEX_CMP_REQUEUE`，再等到超时。
- 修复：每个等待者记录当前 futex 键，requeue 时更新。超时按新键摘队并返回 `ETIMEDOUT`。

### `CLONE_VM` 但不带 `CLONE_THREAD` 时，子进程退出会拆掉共享地址空间里的 SysV 共享内存

- 严重度：高
- 位置：`os/src/kernel/syscall/task/clone_ops.rs` 里非 `CLONE_THREAD` 分支；`os/src/kernel/task/mod.rs` 的 `detach_all_shm`；`os/src/kernel/syscall/task/exec_ops.rs` 也会调用它
- 现象：不带 `CLONE_THREAD` 时，子进程拿到一份附件表的拷贝，并且对表里每个段再 `mark_attached`。地址空间在带 `CLONE_VM` 时仍是同一份。子进程 `exit` 或 `execve` 会按自己的表 `munmap` 这些区间，再按自己的 pid 做 detach。父进程的表还记着这些映射，页已经没了，attach 计数也被多减一次。普通 `fork` 会复制页表，多出来的 attach 计数是对的；`CLONE_THREAD` 共享同一张表，也不会再次 `mark_attached`。
- 触发：父进程已经 `shmat`，然后 `clone(CLONE_VM)`（不要带 `CLONE_THREAD`），子进程再 `exit` 或 `execve`。
- 修复：只有真正新建地址空间时才复制附件并增加 attach 计数。共享地址空间的子进程退出时不要 `munmap` 父进程仍在用的区间。

### `prlimit` / `setrlimit` 改的是 `fork` 之后仍共享的那一份

- 严重度：高
- 位置：`os/src/kernel/syscall/task/clone_ops.rs` 的 `task.rlimit.clone()`；`os/src/kernel/syscall/task/process_ops.rs` 的 `prlimit`
- 现象：`rlimit` 的类型是 `Arc<SpinLock<...>>`。`clone` 只克隆了这个 `Arc`。之后任一进程 `prlimit` 写入 `limits[resource]`，另一进程的 `getrlimit` 会看到新值。`RLIMIT_NOFILE` 只对这次查到的那个任务调用 `fd_table.set_max_fds`，共享同一份限制的其他进程描述符上限并不跟着变。
- 触发：`fork` 之后父进程 `setrlimit(RLIMIT_STACK, ...)` 或 `prlimit` 改 `RLIMIT_NOFILE`，再在子进程里读取。
- 修复：`fork` 时深拷贝限制结构。改 `RLIMIT_NOFILE` 时，更新所有仍共享该描述符表的线程，而不是只改一个 tid。

## 内存管理

### 多页映射中途失败留下悬空页表项

- 严重度：致命
- 位置：`os/src/mm/memory_space/mapping_area/map_ops.rs` 的 `map_one_with_batch`；`os/src/mm/memory_space/space/address_space.rs` 的 `insert_area`
- 现象：帧先插入 `frames`，再装页表项。`area.map()` 失败时整个 area 被丢弃。`MappingArea` 没有 `Drop` 去拆除已经装上的 PTE，帧跟踪器却会释放物理页。
- 触发：多页 `mmap` 或 `brk` 增长在中途分配失败。
- 修复：失败时按已成功的 VPN 拆页表，再释放帧。

### mprotect(PROT_NONE) 释放页内容

- 严重度：高
- 位置：`os/src/mm/memory_space/mapping_area/split_ops.rs` 的 `partial_change_permission`
- 现象：转到 `PROT_NONE` 时解除映射并释放帧。再改回可读时分配新的零页，也不会从文件重新载入。`MAP_SHARED` 上尚未写回的修改一起丢失。`mmap(PROT_NONE)` 用保留区占位，和这条路径不一致。
- 触发：写入匿名映射后 `mprotect(PROT_NONE)`，再改回 `PROT_READ`。
- 修复：`PROT_NONE` 只去掉用户访问权限，帧和文件内容保留。

### 文件映射 mprotect 用页数减字节长度

- 严重度：高
- 位置：`os/src/mm/memory_space/mapping_area/split_ops.rs`
- 现象：右半段 `MmapFile.len` 用 `f.len - 页数 * PAGE_SIZE` 计算，而且在判断右半段是否存在之前就算。`f.len` 是用户请求的字节数，VMA 按页向上取整。映射 100 字节再 `mprotect` 整段时，减数是 4096。调试构建会在内核里溢出 panic。
- 触发：对非页对齐长度的文件映射做 `mprotect`，包括动态链接器的 RELRO。
- 修复：按字节切 `offset` 和 `len`。右半段只在确实还有剩余页时计算。

### munmap 前缀后写回偏移仍是原来的

- 严重度：高
- 位置：`os/src/mm/memory_space/mapping_area/split_ops.rs` 的前缀卸载；`os/src/mm/memory_space/mapping_area/file_ops.rs` 的 `sync_file`
- 现象：卸掉共享映射前半段时只改 `vpn_range`，不增加 `MmapFile.offset`。写回用新的 VMA 起点计算页序号，剩余页的脏数据写到文件原来的开头。
- 触发：`MAP_SHARED` 至少两页，改第二页，`munmap` 第一页，再写回或退出。
- 修复：前缀卸载时把文件 `offset` 加上被卸掉的字节数，并缩短 `len`。

### brk(0) 返回页对齐的 VMA 末尾

- 严重度：中
- 位置：`os/src/mm/memory_space/space/address_space.rs` 的 `current_brk`；`os/src/kernel/syscall/mm.rs` 的 `brk`
- 现象：成功的 `brk(new)` 返回请求值。`brk(0)` 走 `current_brk()`，返回堆 VMA 的页结束地址。内核没有保存精确断点。
- 触发：`brk(heap_start + 100)` 成功后再 `brk(0)`。musl 的 `sbrk` 用 `brk(0)` 当当前断点，堆会跳到下一页。
- 修复：单独保存精确的 brk 字节地址。VMA 仍按页对齐。

### munmap / mprotect 的区间加法没有检查溢出

- 严重度：中
- 位置：`os/src/mm/memory_space/space/mmap_ops.rs`
- 现象：`start + len` 是普通 `usize` 加法。开发配置打开溢出检查时，接近地址空间顶部的长度会在内核里 panic。`mmap` 对 hint 做了溢出检查，这两条路径没有。
- 触发：`munmap` 或 `mprotect` 的起点加长度超过 `usize::MAX`。
- 修复：溢出返回 `EINVAL`，区间还要落在用户地址空间内。

## 路径、VFS 与文件系统

### 创建和删除按字符串折叠 ..

- 严重度：高
- 位置：`os/src/kernel/syscall/util.rs` 的 `split_parent_preserving_basename`
- 现象：`unlinkat`、`renameat2`、`mkdirat`、`open(O_CREAT)` 先对父路径做词法 `normalize_path`，不跟随符号链接。
- 触发：`/tmp/link` 指向 `/etc` 时，`unlink("/tmp/link/../shadow")` 删除 `/tmp/shadow`。逐级解析应先进入 `/etc`，再处理 `..`，目标是 `/shadow`。
- 修复：父目录逐级 walk，只把最后一级名字留着不跟随。

### 绝对路径缓存使用词法路径

- 严重度：高
- 位置：`os/src/vfs/path.rs` 的 `vfs_lookup`
- 现象：绝对路径先 `normalize_path`，再拿这个字符串查 dentry 缓存。命中就直接返回。未命中才按原始分量逐级走，中间符号链接会跟随。同一路径在缓存冷、热时结果不同。
- 触发：先打开 `/etc/passwd` 填上缓存，再访问 `/link/../etc/passwd`，其中 `/link` 指向别的目录。
- 修复：缓存键使用 walk 得到的真实路径。查找仍必须逐级进行。

### 挂载根的 .. 出不去

- 严重度：高
- 位置：`os/src/vfs/path.rs` 的 `resolve_component`；`os/src/vfs/dentry.rs` 的 `mounted_on`
- 现象：`..` 只看 `parent()`。挂载文件系统的根没有父 dentry，只记录了 `mounted_on`。`parent()` 为空时返回自身。`full_path()` 会沿着 `mounted_on` 拼出 `/mnt`，所以 `getcwd` 和 `chdir("..")` 不一致。
- 触发：`chdir("/mnt/..")` 或 `open("/mnt/..")`。
- 修复：挂载根遇到 `..` 时回到挂载点的父目录。

### 块设备跨扇区读写算错扇区号

- 严重度：高
- 位置：`os/src/vfs/impls/blk_dev_file.rs` 的 `read` 和 `write`
- 现象：扇区号按“已拷贝字节 / 512”递增。起始偏移落在扇区中间时，第一段不足 512 字节，下一次仍算出同一个扇区，并把扇区内偏移清成 0。
- 触发：对块设备从偏移 100 读或写 1000 字节。写会覆盖该扇区开头，并漏掉下一个扇区。
- 修复：用当前文件偏移计算扇区号，跨过扇区边界后再把扇区内偏移置 0。

### pread / pwrite 不检查打开方式

- 严重度：高
- 位置：`os/src/vfs/impls/reg_file.rs` 的 `read_at` / `write_at`；`os/src/kernel/syscall/io.rs` 的 `pread64` / `pwrite64`
- 现象：普通 `read` / `write` 检查可读、可写。`read_at` / `write_at` 直接进 inode。
- 触发：`O_RDONLY` 上 `pwrite`，或 `O_WRONLY` 上 `pread`。应返回 `EBADF`。

### ext4 删除名字时无条件释放 inode

- 严重度：高
- 位置：`third_party/ext4_rs/src/ext4_impls/ext4.rs` 的 `unlink`；内核侧 `os/src/fs/ext4/inode.rs` 对普通文件调用它
- 现象：删掉目录项之后总是 `ialloc_free_inode`，不看 `links_count`。两个硬链接删掉其中一个后，剩下的名字指向已释放的 inode，这个号还可以被重新分配。`rename(path, path)` 会先释放唯一 inode，再把同一个号加回去。
- 另外：`rmdir` 把 `dir_remove` 的所有失败都映射成 `ENOENT`。非空目录因此看起来像已经不存在。
- 修复：链接数大于 1 时只减计数并删目录项。源和目标是同一目录项时直接成功。`rmdir` 按非空、不是目录、不存在分别返回错误。

### tmpfs 建不了符号链接

- 严重度：高
- 位置：`os/src/fs/tmpfs/inode.rs` 的 `write_at`、`read_at`、`symlink`、`readlink`
- 现象：`symlink` 创建 `InodeType::Symlink` 后调用 `write_at`。`write_at` 和 `read_at` 拒绝一切非普通文件，并返回 `EISDIR`。写入发生在插入目录项之前，链接不会留下。`readlink` 同样走 `read_at`。
- 触发：在 tmpfs 上 `symlink`。
- 修复：符号链接单独保存目标字符串。

### 读成功后的用户拷贝失败被忽略

- 严重度：高
- 位置：`os/src/kernel/syscall/io.rs` 的 `read` 和 `pread64`
- 现象：管道数据已经从环形缓冲弹出，普通文件的偏移也已经增加。随后 `copy_to_user` 的错误被 `.ok()` 吃掉，系统调用仍返回正数。
- 触发：`read(fd, NULL, n)`，或缓冲区未映射，而 fd 里已经有数据。
- 修复：先确认用户缓冲可写。管道在拷贝成功后再从环形缓冲取出。

### 写端已关闭的管道不发送 SIGPIPE

- 严重度：中
- 位置：`os/src/vfs/impls/pipe_file.rs`
- 现象：读端已经关闭时只返回 `EPIPE`。默认动作还应先投递 `SIGPIPE`。忽略 `write` 返回值的程序不会被杀掉。
- 修复：返回 `EPIPE` 之前向当前进程发送 `SIGPIPE`。

### 管道有一点空间就短写，阻塞写不是原子的

- 严重度：中
- 位置：`os/src/vfs/impls/pipe_file.rs` 的 `write`
- 现象：剩余空间大于 0 时立刻写入 `min(请求长度, 剩余空间)` 并返回。只有完全没有空间才返回 `EAGAIN`，由系统调用层重试。长度不超过 `PIPE_BUF` 的阻塞写因此可以被拆开，和其他写者交错。
- 触发：管道里只剩 10 字节，阻塞地写入 100 字节。
- 修复：阻塞且长度不超过 `PIPE_BUF` 时，等剩余空间够整段再写。

### ppoll 会丢掉唤醒，pselect 和管道锁顺序相反

- 严重度：高；多核上 `pselect` 与管道读写同时发生时可以死锁
- 位置：`os/src/kernel/syscall/io.rs` 的 `poll_with_timeout`、`select_common`、`wake_poll_waiters`；`os/src/vfs/impls/pipe_file.rs` 的读写
- 现象：`ppoll` 先检查就绪并放开相关锁，再调用 `POLL_WAIT_QUEUE.sleep()`。这中间到达的数据会 `wake_up_all()`，当时队列是空的，之后 `ppoll` 无限等待就不再醒来。`pselect` 用 `sleep_if`，在持有 `POLL_WAIT_QUEUE` 时调用 `check_fds`，管道的 `read_ready` 会再锁管道缓冲。管道写路径是先锁缓冲，再 `wake_poll_waiters()` 去锁 `POLL_WAIT_QUEUE`。两把锁的顺序相反。
- 触发：`ppoll` 无限等待管道，对端在检查之后、睡眠之前写入。或多核上一个核停在 `pselect` 的就绪检查里，另一个核同时写同一条管道。
- 修复：先放下管道锁再唤醒等待队列。睡眠必须在持有等待队列锁时重查就绪。`ppoll` 和 `pselect` 还应在睡眠前套上调用者给出的临时信号掩码。

### dup2 / dup3 先关掉目标描述符再安装

- 严重度：高
- 位置：`os/src/vfs/fd_table.rs` 的 `dup3`
- 现象：先 `close(new_fd)`，再 `install_at_with_flags`。安装会在 `new_fd >= max_fds` 时失败。失败后目标描述符已经没了，POSIX 要求失败时 `newfd` 保持原样。`CLONE_FILES` 时，关闭和安装之间另一个线程还可能把这个槽位分配走。
- 触发：打开 fd 200，把 `RLIMIT_NOFILE` 降到 100，再 `dup2(1, 200)`。
- 修复：在同一把锁里检查上限并替换槽位。失败路径不要先关闭。

### POSIX 文件锁不拆区间，进程退出也不释放

- 严重度：高
- 位置：`os/src/vfs/file_lock.rs` 的 `set_lock`；`release_all_locks` 没有任何调用者
- 现象：解锁时把同一 pid 任何重叠记录整段删掉，不会把左右两段留下来。`[0,100)` 上解锁 `[10,20)` 会清掉整把锁。全仓库没有调用 `release_all_locks`，`close` 也不碰这张表。进程退出后锁还在；pid 复用后，新进程带着别人的锁，其他进程一直 `EAGAIN`。
- 触发：`F_SETLK` 锁到文件尾后进程退出，另一个进程再锁同一文件。
- 修复：按区间拆分和合并。`close` 释放该打开文件上本进程的 POSIX 锁，`exit` 调用 `release_all_locks`。

### sendfile 出错时不更新 offset，短写会丢掉已读数据

- 严重度：高
- 位置：`os/src/kernel/syscall/io.rs` 的 `sendfile`
- 现象：读或写失败时直接返回，函数末尾写回 `*offset` 的代码被跳过。部分成功后再出错，用户看到的偏移仍是调用前的值，重试会再复制同一段。`offset == NULL` 时用 `read()`，输入偏移已经加上本次读出的长度；输出若短写，多读出来的字节不在任何缓冲区里。
- 触发：`sendfile` 到管道或套接字，第二次写入返回错误或短写。
- 修复：输入使用 `read_at`。每成功写出一段就更新偏移，错误返回前也要写回。

### getdents64 一条都放不下时返回 0

- 严重度：高
- 位置：`os/src/kernel/syscall/fs/stat_ops.rs` 的 `getdents64`
- 现象：缓冲区放不下一笔记录就 `break`。一笔都没写时返回 0。0 对调用者表示目录结束。偏移也不前进，下次还是放不下。`"."` 的记录长度至少 24 字节。
- 触发：`getdents64(dirfd, buf, 16)`。
- 修复：还有目录项但一条都写不下时返回 `EINVAL`。用户缓冲整段写成功后再推进目录偏移。

### 目录可以按可写文件打开，ftruncate 会截断 ext4 目录

- 严重度：高
- 位置：`os/src/kernel/syscall/fs/path_ops.rs` 的 `openat`；`os/src/kernel/syscall/fs/fd_ops.rs` 的 `ftruncate`；`os/src/fs/ext4/inode.rs` 的 `truncate`
- 现象：`openat` 只在带 `O_DIRECTORY` 时检查类型，不拒绝以 `O_WRONLY` 或 `O_RDWR` 打开目录。`O_TRUNC` 有普通文件判断，`ftruncate` 只看 `file.writable()`。ext4 的 `truncate` 不看 inode 类型，直接 `truncate_inode`。
- 触发：`open("/tmp", O_RDWR)` 然后 `ftruncate(fd, 0)`。
- 修复：对目录请求写权限时返回 `EISDIR`。`ftruncate` 和 ext4 `truncate` 拒绝非普通文件。

### readv / writev 跳过空指针，阻塞描述符也直接返回 EAGAIN

- 严重度：高
- 位置：`os/src/kernel/syscall/io.rs` 的 `readv` 和 `writev`
- 现象：`iov_base == NULL` 时直接 `continue`，后面的 iovec 继续读写。非零长度配空指针应返回 `EFAULT`。管道返回 `EAGAIN` 时立即结束，没有 `read` / `write` 那条阻塞重试。
- 触发：`readv` 的第一段是 `{NULL, 100}`，第二段是真实缓冲。或对默认阻塞的管道调用 `readv`。
- 修复：非零长度且基址为空时返回 `EFAULT`。阻塞描述符上的 `EAGAIN` 与 `read` / `write` 一样重试。

### O_APPEND 只串行化同一个打开文件

- 严重度：高
- 位置：`os/src/vfs/impls/reg_file.rs` 的 `write`
- 现象：追加时在这个 `RegFile` 的偏移锁里读 inode 大小，再 `write_at`。锁不住另一个 `open(O_APPEND)` 得到的文件对象。两次打开可以读到同一个大小，写到同一位置，后写的覆盖先写的。
- 触发：两个描述符同时以 `O_APPEND` 写同一个文件。
- 修复：在 inode 锁里完成读大小、写入和更新大小。同一把锁也要盖住 `ftruncate`。

### `poll` / `ppoll` 超时为 0 时仍会睡眠

- 严重度：中
- 位置：`os/src/kernel/syscall/io.rs` 的 `poll` 和 `poll_with_timeout`
- 现象：`timeout == 0` 被做成全零的 `TimeSpec`。换算成 tick 是 0，到期点等于当前的 `get_time()`。没有就绪描述符时仍然放进定时器队列并 `sleep`。`select` 在到期点已经到了的时候会直接返回，这条路径没有同样的判断。不足一个 tick 的超时被整数除法收成 0 后也一样，要睡到下一次时钟中断。
- 触发：`poll(fds, nfds, 0)`，当时没有描述符就绪。
- 修复：到期点小于等于当前时间时，检查一遍就返回 0，不要入队。

## 用户内存拷贝

### read_from_user 失败后仍假定初始化完成

- 严重度：高
- 位置：`os/src/util/user_buffer.rs` 的 `read_from_user` 和 `write_to_user`
- 现象：`copy_from_user` 失败被 `.ok()` 吃掉，未写入的 `MaybeUninit` 接着 `assume_init()`。`write_to_user` 同样吞掉错误。`pipe2` 因此在用户指针非法时仍返回 0，描述符已经分配出去。`futex` 超时、`sigaction` 等读用户结构体的路径会拿到未初始化的值。
- 修复：改成返回 `Result`。失败不要把未初始化内存当成有效值。

## 网络

### virtio-net 接收队列初始化长度是 0，收到的 buffer 不归还

- 严重度：致命（网卡不可用；若只改长度则会在收满后永久停收）
- 位置：`os/src/device/net/net_device.rs`
- 现象：`VirtIONet::new(transport, 0)` 的第二个参数是每个 RX buffer 的字节数。0 低于 virtio-drivers 0.12 要求的最小以太网缓冲，初始化失败后设备被跳过。`receive` 拷贝完包就丢弃 `RxBuffer`，没有 `recycle_rx_buffer`。256 个槽用完后描述符不会回到接收队列。
- 修复：缓冲区长度至少取驱动要求的最小值。拷贝完成后把 buffer 归还接收队列。

### 发送路径用 IP MTU 截断以太网帧

- 严重度：高
- 位置：`os/src/device/net/net_device.rs` 的 `send`；`os/src/net/stack/adapter.rs` 的 `NetTxToken`
- 现象：设备 MTU 固定 1500。协议栈宣布的最大传输单元是 `mtu + 以太网头 14`。完整帧最长 1514 字节，被 `send` 拒绝。`TxToken` 把这个错误丢掉。
- 触发：IP 载荷接近 1500 字节的 TCP 或 UDP，例如 iperf。
- 修复：允许帧长到 `mtu + 14`。发送失败要传回协议栈。

### socket 句柄按 (tid, fd) 索引

- 严重度：高
- 位置：`os/src/net/socket.rs` 的 `FD_SOCKET_MAP`；`os/src/kernel/syscall/network/connection_ops.rs` 的 `connect`
- 现象：`bind`、`connect`、`listen`、`accept` 用当前任务的 tid 查表，查不到返回 `ENOTSOCK`（-88）。`fork`、`dup` 和 `CLONE_FILES` 的其他线程拿到的是同一个打开文件，tid 不同。另一个线程 `close` 时按自己的 tid 删除，创建者的表项还在。
- 触发：prefork 服务器在子进程里 `accept`；线程 A `socket`，线程 B `bind`。
- 修复：句柄只放在 `SocketFile` 里。

### AF_UNIX 对同一把锁再加锁

- 严重度：高
- 位置：`os/src/net/unix_socket.rs` 的 `connect_stream`
- 现象：先锁监听 socket 的状态，再锁自己的状态。对自己 `connect` 时两者是同一个对象，`SpinLock` 不可重入，后面的 `EINVAL` 执行不到。两个流 socket 互相 `connect` 时加锁顺序相反，也会死锁。
- 修复：先判断是不是同一个 socket。跨 socket 时按地址顺序加锁。

### getifaddrs 链表从第二个节点起断开

- 严重度：高
- 位置：`os/src/kernel/syscall/network/ifaddrs_ops.rs`
- 现象：第一个 `ifaddrs` 写在内核缓冲偏移 0。链接前驱时用 `prev_ifaddrs_offset != 0` 判断有没有前驱。0 同时是第一个节点的真实偏移，所以第一条的 `ifa_next` 不会被写成第二个节点。
- 触发：`lo` 和 `eth0` 同时存在，或一个接口有多个地址，然后 `getifaddrs`。
- 修复：用 `Option<usize>` 记录前驱。

### UDP 数据报在进队列前被截成 2048 字节

- 严重度：高
- 位置：`os/src/net/stack/mod.rs` 的 `udp_dispatch_drain_locked`；上限是 `os/src/net/socket.rs` 的 `UDP_DGRAM_MAX`
- 现象：从 smoltcp 取出完整载荷后，只拷贝 `min(长度, 2048)`，并把队列里的 `len` 记成截断后的值。用户缓冲更大时，`recvfrom` 也看不到尾部，返回值不是原始长度，也没有 `MSG_TRUNC`。
- 触发：向已绑定的 UDP 口发送超过 2048 字节的数据报。
- 修复：按数据报实际长度保存。拷进用户缓冲时再截断，并让调用方能看出原始长度。

### UDP 接收队列满时丢掉尚未读取的队头

- 严重度：高
- 位置：`os/src/net/socket.rs` 的 `udp_push`
- 现象：队列长度达到上限，或扩容失败后仍然放不下时，先 `pop_front()` 再 `push_back`。函数仍返回成功。已经排队、应用下一拍就要读的数据报被扔掉，新来的留下。
- 触发：一个 UDP socket 收得比读得快，队列超过 `UDP_RXQ_MAX_CAP`。
- 修复：放不下时丢掉新数据报，不要动队头。

### 同一端口可以绑多次，后一次的地址被丢掉

- 严重度：高
- 位置：`os/src/net/stack/mod.rs` 的 `udp_attach_fd_to_port`；`SO_REUSEADDR` 只在 `os/src/kernel/syscall/network/sockopt_ops.rs` 里写入 `SocketOptions`
- 现象：端口已经在 `udp_ports` 里时，直接复用已有的 smoltcp socket，不再用这次的 `bind_addr` 做 `bind`。`os/src/net` 里没有读取 `reuse_addr` 或 `reuse_port` 的地方。没有设置复用选项时，第二个 `bind` 也不会返回 `EADDRINUSE`。先绑一个具体地址，再绑 `0.0.0.0` 的同一端口，协议栈仍只收第一次那个地址。
- 触发：两个 socket 先后 `bind` 同一个 UDP 端口。
- 修复：默认冲突返回 `EADDRINUSE`。只有双方都设置了对应复用选项才共享端口，并按目的地址选择最具体的 socket。

### `TCP_NODELAY` 写进选项结构后没有交给协议栈

- 严重度：高
- 位置：`os/src/kernel/syscall/network/sockopt_ops.rs`；`os/src/net/socket.rs` 的 `set_socket_options`；`os/src/net/stack/mod.rs` 的 `create_tcp_socket`
- 现象：`setsockopt(IPPROTO_TCP, TCP_NODELAY)` 成功返回，只把 `SocketOptions.tcp_nodelay` 写成 true。创建 smoltcp TCP socket 之后没有 `set_nagle_enabled`。全仓库也没有把这个字段读出来用。小包仍按 Nagle 打开时的方式发送。
- 触发：`setsockopt` 打开 `TCP_NODELAY` 后连续 `send` 很短的数据。
- 修复：创建 socket 和后来改选项时，把 Nagle 开关写进 smoltcp socket。

### 未连接或被重置的 TCP 一直可读，`shutdown(SHUT_RD)` 则永远不可读

- 严重度：高
- 位置：`os/src/net/stack/mod.rs` 的 `socket_readable`、`socket_read`；`os/src/net/socket.rs` 的 `shutdown_read`
- 现象：非监听 TCP 在 `can_recv` 之外，把 `Closed` 和 `CloseWait` 都当成可读。新建的 smoltcp TCP socket 就是 `Closed`，所以还没 `connect` 时 `poll(POLLIN)` 立刻返回，接着 `read` 返回 0。对端 RST 进入 `Closed` 时同样返回 0，调用方看不到 `ECONNRESET`。`shutdown(SHUT_RD)` 只把本地标志设为 true：`socket_read` 会返回 0，但 `socket_readable` 不看这个标志，也不会唤醒 `poll`。接收队列里已有的数据留在协议栈里。`CloseWait` 且接收队列为空时返回 0，这一种和正常 FIN 一致。
- 触发：对刚 `socket` 出来的 TCP 做 `poll(POLLIN, -1)`；或一个线程 `poll`，另一个线程 `shutdown(fd, SHUT_RD)`。
- 修复：只有对端已经关闭或本端 `SHUT_RD` 时才把“读到 0”当成可读，并在 `shutdown` 时唤醒等待者。从未连接以及被 RST 的 `Closed` 按错误返回，不要伪装成 EOF。

### 监听队列里已经被 RST 的连接不释放 smoltcp socket

- 严重度：中
- 位置：`os/src/net/stack/mod.rs` 的 `take_established_from_listen_queue`、`take_spare_tcp_listener`
- 现象：队列项的状态是 `Closed` 时，只把它从 `Vec` 里删掉，持有的 `SocketSet` 锁没有用来 `sockets.remove`。这个 handle 继续占着协议栈里的位置。`tcp_spare_listener_count` 走的是同一类删除。
- 触发：对监听端口发 SYN，在 `accept` 之前对端 RST，并反复这样做。
- 修复：从队列剔除 `Closed` 时同时从 `SocketSet` 删除。

### AF_UNIX 监听队列满时，阻塞 `connect` 也返回 `EAGAIN`，`accept` 继承客户端的非阻塞标志

- 严重度：高
- 位置：`os/src/net/unix_socket.rs` 的 `connect_stream`
- 现象：`pending.len() >= backlog` 时直接返回 `-EAGAIN`，不看 `O_NONBLOCK`，也不睡觉等 `accept`。随后 `new_connected_stream` 使用的是客户端自己的 `OpenFlags`。客户端带 `O_NONBLOCK` 时，`accept` 得到的描述符也是非阻塞的。
- 触发：阻塞的 AF_UNIX 客户端在满队列上 `connect`；或非阻塞客户端连上后，服务端 `accept` 再 `read`。
- 修复：阻塞 socket 在队列有空位或被信号打断之前等待。`accept` 出来的 socket 使用空的打开标志，除非调用的是 `accept4(SOCK_NONBLOCK)`。

### 关闭监听 socket 时，尚未 accept 的连接被直接摘掉

- 严重度：高
- 位置：`os/src/net/stack/mod.rs` 的 `drop_socket_file`；同一条 `SocketFile.listen_sockets` 也由 `take_established_from_listen_queue` 取出已经建立的连接
- 现象：主句柄处于 `Closed`、`TimeWait` 或 `Listen` 时直接 `sockets.remove`。其他状态调用 `socket.close()` 后放进 `pending_tcp_close`，走正常关闭流程。随后队列里的每个句柄都只做 `sockets.remove`，没有 `close`，也没有 `abort`。队列里既有仍在 `Listen` 的后备 socket，也有已经 `Established` 或还在握手、还没被 `accept` 取走的连接。后两类从协议栈消失时，这次关闭不会主动向对端发送 FIN 或 RST；不能据此断言对端后续发包也永远收不到 RST。这和「队列里 `Closed` 的项不从 `SocketSet` 删除」是两条。
- 触发：`listen` 之后有连接进入 accept 队列，服务端在 `accept` 之前 `close` 监听描述符。
- 修复：未接受的连接调用 `abort`，并保留句柄直到协议栈实际发送 RST，再回收。smoltcp 0.12 的 `abort` 只切换到 `Closed`，RST 在后续 `dispatch` 中发送；不能立即删除，也不能让当前按 `Closed` 回收的队列提前删掉它。仍在 `Listen` 的后备 socket 可以直接删除。依据：[smoltcp TCP 实现][verify-smoltcp-tcp]。

### `socketpair` 的 UNIX 数据报 `sendto` 忽略目的地址

- 严重度：高
- 位置：`os/src/net/unix_socket.rs` 的 `send_datagram`；`create_unix_socket_pair` 只设置 `dgram_peer`
- 现象：`dgram_peer` 还能升级成对端时，数据报放进那个对端的队列，参数里的目的地址不参与查找。`socketpair(SOCK_DGRAM)` 会设置 `dgram_peer`。普通 `connect` 只写 `peer_addr`，之后的 `sendto` 仍按传入地址查找。
- 触发：`socketpair` 得到一对数据报 socket，对其中一个 `sendto` 另一个已经绑定的路径。
- 修复：显式目的地址优先，按该地址查找并投递；没有目的地址时才使用 `dgram_peer` 或默认对端，不能统一返回 `EISCONN`。宿主机用例确认配对端未收到数据，指定的第三个数据报 socket 收到数据。依据：[Linux UNIX 数据报发送实现][verify-linux-unix]。

### 流式 `sendto` 的目的地址处理不符合对应协议语义

- 严重度：高
- 位置：`os/src/net/unix_socket.rs` 的 `send_to`；`os/src/net/stack/mod.rs` 的 `socket_sendto`；`os/src/kernel/syscall/network/addr_ops.rs` 的 `sendto`
- 现象：AF_UNIX 的 `SOCK_STREAM` 丢掉地址，直接 `write`。已连接时数据进入当前连接，未连接时变成未连接错误。TCP 只要 `sendto` 带了非空地址，`socket_sendto` 对 TCP 句柄返回 `NotSupported`，系统调用得到 `EOPNOTSUPP`。地址指针为空时 `sendto` 改走 `send`。
- 触发：对已连接的 UNIX 流 `sendto` 另一个地址，数据仍进当前连接；对已连接的 TCP `sendto` 一个地址，得到 `EOPNOTSUPP`。
- 修复：分别处理 AF_UNIX 和 TCP。UNIX 流带目的地址时，已连接返回 `EISCONN`，未连接返回 `EOPNOTSUPP`；没有目的地址且未连接时才是 `ENOTCONN`。已连接 TCP 忽略目的地址，向当前连接发送，不能统一报 `EISCONN` 或 `EOPNOTSUPP`。这些分支已用宿主机最小用例核对；UNIX 分支依据：[Linux UNIX 流发送实现][verify-linux-unix]。

### `recvfrom` 把 `addrlen` 写成实际拷贝的字节数

- 严重度：高
- 位置：`os/src/kernel/syscall/network/addr_ops.rs` 的 `recvfrom`。对照 `os/src/net/socket.rs` 的 `write_sockaddr_in` 和 `os/src/net/unix_socket.rs` 的 `write_sockaddr_un`
- 现象：载荷已经由 `file.recvfrom` 取出之后，源地址按 `min(用户给出的长度, 内核地址长度)` 拷贝，再把这个拷贝长度写回 `*addrlen`。`getsockname`、`getpeername`、`accept` 写回的是未截断的内核地址长度。调用方看不出缓冲区是不是太小。
- 触发：`recvfrom` 时 `*addrlen` 小于 `sockaddr_in`，或小于实际的 `sockaddr_un`。
- 修复：保存用户传入的容量，按该容量截断地址拷贝，并回写完整地址长度；检查用户拷贝是否失败。宿主机 IPv4 UDP 用例给出 2 字节地址缓冲，返回的完整长度为 16。

### 套接字拷贝失败仍返回成功，接收队列已经被消费

- 严重度：高
- 位置：`os/src/kernel/syscall/network/connection_ops.rs` 的 `send`、`recv`；`os/src/kernel/syscall/network/addr_ops.rs` 的 `sendto`、`recvfrom`
- 现象：`copy_from_user` 和 `copy_to_user` 的失败被 `.ok()` 丢掉。`send` / `sendto` 可能把没有拷全的缓冲区发出去，并返回字节数。`recv` / `recvfrom` 先从 TCP、UDP 或 UNIX 队列取出数据，拷贝失败后仍返回取出的长度。`read` / `pread64` 上已经记录的用户拷贝问题是另外的系统调用。
- 触发：缓冲区指针非法，或长度跨过未映射页。
- 修复：检查拷贝结果；完全失败应返回 `EFAULT`，不得把未拷入的内容发送出去，也不得把未交付的数据长度当作成功返回。接收失败后的队列处理要按协议和部分成功语义设计，不能把「必须保留数据」当作 Linux 保证：宿主机 UDP 用例在非法接收指针上返回 `EFAULT`，该数据报同样已被消费。本条确认的是错误被忽略，不是消费队列本身。

### `MSG_PEEK` 会把数据从队列里删掉

- 严重度：高
- 位置：上面四个系统调用的 `flags` 参数没有被读取。UNIX 数据报在 `read` / `recvfrom` 里 `pop_front`。TCP 走 `recv_slice`。UDP 走 `udp_pop`
- 现象：`MSG_PEEK` 仍然消费这次读到的数据。下一次读看不到同一段。
- 触发：`recv(fd, buf, len, MSG_PEEK)` 之后再 `recv` 一次。
- 修复：`MSG_PEEK` 只复制，不推进接收队列。

### TCP `connect` 把正在连接当成已经连接，并覆盖 `bind` 的地址

- 严重度：高
- 位置：`os/src/kernel/syscall/network/connection_ops.rs` 的 `connect`；`os/src/net/stack/mod.rs` 的 `tcp_debug_state`、`socket_local_endpoint`；`os/src/kernel/syscall/network/socket_ops.rs` 的 TCP `bind`；默认地址在 `os/src/net/config.rs` 的 `init_default_interface`
- 现象：`connect` 在调用 `tcp_connect` 之前，只要 `socket.is_open()` 为真就返回 `-106`（`EISCONN`）。smoltcp 0.12 的 `is_open` 在状态不是 `CLOSED` 和 `TIME-WAIT` 时为真，因此 `SYN_SENT` 上的第二次 `connect` 也是 `EISCONN`，得不到 `EALREADY`。本地端点只问 smoltcp。TCP `bind` 只把地址记在 `SocketFile` 上，连接建立前 smoltcp 没有本地端点，于是使用缺省值：目的地址首字节是 127 时用 `127.0.0.1`，否则用 `10.0.2.15`；端口为 0 时再分配临时端口，并写回 `SocketFile`。网卡默认地址是 `192.168.1.100/24`。先前 `bind` 的地址和端口不会传给 `tcp_connect`。
- 触发：非阻塞 `connect` 在 `SYN_SENT` 时再调一次；或先 `bind` 到 `192.168.1.100` 的某个端口，再 `connect` 外网地址。
- 修复：区分连接中和已连接，`SYN_SENT` 上的重复调用返回 `EALREADY`，已连接时返回 `EISCONN`。已经绑定的端点优先使用。未绑定时用出接口地址，不要写死 `10.0.2.15`。

### 两个 TCP socket 可以 `listen` 同一个端口

- 严重度：高
- 位置：`os/src/kernel/syscall/network/socket_ops.rs` 的 `bind`、`listen`；`os/src/net/stack/mod.rs` 的 `tcp_listen`
- 现象：TCP `bind` 只检查这个 `SocketFile` 自己有没有端口。`listen` 只对当前句柄调用 smoltcp 的 `listen`。该调用只检查当前 socket 的状态和端口是否为 0；同一 socket 已在相同端点监听时也允许成功。错误被映射成 `AddressInUse`，但不会检查其他 socket 是否占用端点。另一个仍处于 `Closed` 的 socket 对同一端口再 `listen` 会成功。依据：[smoltcp TCP 实现][verify-smoltcp-tcp]。
- 触发：两个 TCP socket 先后 `bind` 再 `listen` 同一个端口。
- 修复：没有对应的复用选项时返回 `EADDRINUSE`。`SO_REUSEADDR` 和 `SO_REUSEPORT` 目前只写进 `SocketOptions`，协议栈没有读取。

### UNIX 数据报 `connect` 不查找对端，发给流式 socket 的数据报没人读

- 严重度：高
- 位置：`os/src/net/unix_socket.rs` 的 `connect`、`send_datagram`、`enqueue_datagram`、`lookup_bound_socket`
- 现象：数据报 `connect` 把地址存进 `peer_addr` 就返回 0，不查找绑定表。不存在的路径也能连接成功。发送时找到表里的 socket 就入队，不看它是不是 `SOCK_DGRAM`。流式 socket 的 `read` 只看字节流。发给已经 `listen` 的流式路径时，`sendto` 返回成功，接收方读不到这些字节。
- 触发：数据报 `connect` 一个不存在的路径；或 `sendto` 一个流式监听路径。
- 修复：`connect` 和发送都解析目的地址并检查类型。文件系统路径不存在时返回 `ENOENT`；路径存在但没有可连接的 socket 时返回 `ECONNREFUSED`，缺失的抽象地址也返回 `ECONNREFUSED`。本条发往流式路径的类型不符返回 `EPROTOTYPE`。不能把所有查找错误都归成 `ECONNREFUSED`。依据：[Linux UNIX 地址查找实现][verify-linux-unix]。

### UNIX 路径名和绑定表各记一份

- 严重度：高
- 位置：`os/src/net/unix_socket.rs` 的 `bind`、`create_socket_node`、`Drop`。`UNIX_BINDINGS` 只在这个文件里更新
- 现象：`bind` 到路径时先 `mknod` 一个 `S_IFSOCK` 节点，再把 socket 放进按路径字符串索引的绑定表。`unlink` 只删目录项，不改变这份索引。旧 socket 仍活着时，流式 `connect` 和数据报 `sendto` 仍可按已删除路径找到它；此时再 `bind` 同一路径，会先重建节点，再因旧弱引用仍可升级而返回 `EADDRINUSE`。关闭后路径仍在、未 `unlink` 就再次绑定失败，本身符合 Linux 语义，不是缺陷。
- 触发：路径 socket `bind` 后保持打开，`unlink` 该路径，再按路径连接、发送或重新 `bind`。宿主机对照中，连接已删除路径返回 `ENOENT`，同名重新绑定成功。
- 修复：路径查找应通过 VFS 当前节点关联 socket，区分旧节点与同名新节点。`unlink` 后旧 socket 和已有连接仍可存活，但该名字不再指向它，并允许新 socket 绑定同名新节点。最后一个引用关闭时不要自动删除路径；失败的绑定也应回滚自身创建的节点。依据：[Linux UNIX 路径查找与绑定实现][verify-linux-unix]。

### `getpeername` 把没有记下的对端当成未连接

- 严重度：高
- 位置：`os/src/kernel/syscall/network/addr_ops.rs` 的 `getpeername`；`os/src/net/unix_socket.rs` 的 `create_unix_socket_pair`、`connect_stream`
- 现象：`peer_addr` 为空时返回 `-107`（`ENOTCONN`）。`socketpair` 只建立流的连接状态，或只设置数据报的 `dgram_peer`，两边的 `peer_addr` 都是空的。`accept` 出来的服务端把 `peer_addr` 设成客户端的 `local_addr`；客户端没有 `bind` 时这个值是空的。连接已经建立，`getpeername` 仍报未连接。本地地址为空时，`getsockname` 写地址族和长度 2，那一条是对的。
- 触发：`socketpair` 之后 `getpeername`；或客户端不 `bind` 就 `connect`，服务端 `accept` 之后 `getpeername`。
- 修复：`socketpair` 记下对端。客户端未绑定时，接受端返回长度为地址族宽度的 `AF_UNIX` 地址。

### UNIX 数据报的可写判断不看队列

- 严重度：高
- 位置：`os/src/net/unix_socket.rs` 的 `writable`；队列上限在 `enqueue_datagram` 的 `DGRAM_QUEUE_CAPACITY`
- 现象：数据报只有在 `dgram_peer` 或 `peer_addr` 有值时才报告可写，不看对端队列还剩多少空间。未连接、也不是 `socketpair` 的数据报，`poll(POLLOUT)` 一直等不到，但 `sendto` 一个已绑定地址可以成功。已经连接之后，对端队列满时仍报告可写；随后的非阻塞 `send` 返回 `EAGAIN`，阻塞 `send` 则在系统调用层循环等待。
- 触发：未连接的 UNIX 数据报上只等 `POLLOUT`；或对端不读，本端设置 `O_NONBLOCK`，在队列已满时 `poll(POLLOUT)` 之后 `send`。
- 修复：未连接的数据报按可写报告。已连接时看对端队列是否还放得下。

### 只有首字节为 127 的帧才会环回

- 严重度：高
- 位置：`os/src/net/stack/adapter.rs` 的 `NetTxToken::consume`
- 现象：以太网类型 `0x0800` 时看源、目的地址首字节 `buffer[26]` 和 `buffer[30]` 是否至少一个为 127。ARP `0x0806` 看 `buffer[28]` 和 `buffer[38]`。有真实网卡、源地址也不在 127 网段时，发往本机接口地址 `192.168.1.100` 的帧仍走网卡发送，不会被这条适配层路径放进本机环回队列。
- 触发：配置真实网卡及上述地址，使用非 127 源地址访问该地址上的本机服务。127 网段会进环回队列；无真实网卡的显式 loopback 设备不属于这个触发条件。
- 修复：目的地址属于本机任一接口时环回。

## 调度与时钟

### 睡着之前被唤醒，同一个任务会在两颗 CPU 上跑

- 严重度：致命
- 位置：`os/src/kernel/scheduler/mod.rs` 的 `sleep_task`、`wake_up_task`、`schedule`；`os/src/kernel/scheduler/rr_scheduler.rs` 的 `next_task`
- 现象：`sleep_task` 只把状态改成 `Interruptible` 并移出队列，真正切换要等后面的 `yield_task`。这中间另一侧 `wake_up_task` 看到的不是 `Running`，于是把状态改回 `Running`，按亲和性放进某一颗 CPU 的队列，并可能改掉 `on_cpu`。原 CPU 上如果本地队列是空的，`schedule` 看到“队列空且当前仍是 `Running`”就直接返回，任务继续在旧核上执行；新核上的队列里还有同一份上下文。原 CPU 队列不空时，`next_task` 又会因为 `still_running` 把这个任务再入队一次。两颗 CPU 会同时切换到同一个 `Context`。
- 触发：多核上，`nanosleep`、互斥锁或 `poll` 在 `sleep_task` 和 `yield_task` 之间被另一颗 CPU 叫醒。
- 修复：正在某颗 CPU 上执行的任务只给那颗 CPU 记一次“需要重新调度”。等它真正切走时再入队。不要在它还是 `current` 时改 `on_cpu` 或把它放进另一颗 CPU 的队列。

### idle 被放进普通运行队列

- 严重度：高
- 位置：`os/src/kernel/scheduler/rr_scheduler.rs` 的 `next_task`；任务初始 `vruntime` 在 `os/src/kernel/task/task_struct.rs`
- 现象：从 idle 切走时，idle 的状态仍是 `Running`，于是走 `enqueue_task`。空队列才会专门切到 idle；一旦 idle 在队列里，`is_empty()` 就不再成立。idle 的 `vruntime` 从 0 开始，公平队列会优先选它，然后它在 `wfi` 里占一个时间片。
- 触发：启动后第一次从 idle 切换到 init。单核就会发生。
- 修复：切走本 CPU 的 idle 时不要入队。选择下一个任务时也要跳过 idle。

### 其他核上的线程被标成僵尸后仍继续执行

- 严重度：高
- 位置：`os/src/kernel/scheduler/mod.rs` 的 `exit_task`；`os/src/arch/riscv/trap/trap_handler.rs` 的 `check_timer`
- 现象：退出只把目标从它当时的运行队列摘掉，并改成 `Zombie`，不发重调度 IPI。正在用户态执行的线程本来就不在队列里。时钟中断只有在“时间片用完且队列非空”时才 `schedule`。那颗 CPU 上没有其他可运行任务时，中断返回后继续执行这个已经退出的线程。
- 触发：两个线程分在两颗 CPU 上，其中一个调用 `exit_group`，另一个是该核上唯一还在用户态跑的线程。
- 修复：向仍在执行该线程的 CPU 发重调度 IPI。当前任务已经是 `Zombie` 时立刻切到下一个任务或 idle，不要等队列里先有别的任务。

### clock_nanosleep 的绝对 CLOCK_REALTIME 用开机周期数比较

- 严重度：高
- 位置：`os/src/kernel/syscall/task/time_ops.rs` 的 `clock_nanosleep`
- 现象：`TIMER_ABSTIME` 时，`CLOCK_REALTIME` 和 `CLOCK_MONOTONIC` 都把 `timespec` 用 `into_freq` 转成 tick，直接当作 `TIMER_QUEUE` 的到期点。队列和 `get_time()` 比较，那是开机以来的周期数。墙上时间的绝对 tick 落在几十年之后。相对睡眠用的是 `get_time() + 时长`，这条是对的。另外 `rem == NULL` 时，提前醒来也返回 0，不会返回 `EINTR`。
- 触发：`clock_nanosleep(CLOCK_REALTIME, TIMER_ABSTIME, &ts, NULL)`，`ts` 是当前墙上时间再加一秒。
- 修复：绝对的 realtime 先换成相对剩余时间，再写成 `get_time() + 剩余周期`。被信号打断时无论 `rem` 是否为空都返回 `EINTR`。

### 每颗 CPU 的时钟中断都给 TIMER_TICKS 加一

- 严重度：中
- 位置：`os/src/arch/riscv/trap/trap_handler.rs` 的 `check_timer`
- 现象：每颗 hart 的时钟中断都会 `TIMER_TICKS.fetch_add(1)`。在线 N 核时，这个计数大约比真实 tick 快 N 倍。`clock_gettime` 走的是 mtime，不受这一处影响。
- 触发：SMP 下读取基于 `get_ticks()` 的运行时间。
- 修复：只让启动核计数，或改成 `get_time()` 除以每秒周期数。

## 陷阱、上下文与程序加载

### 处理系统调用时 `sscratch` 仍是用户 `a0`，内核里的缺页会 panic

- 严重度：致命
- 位置：`os/src/arch/riscv/trap/trap_entry.S` 的 `trap_entry` 和 `__restore`；`os/src/arch/riscv/trap/trap_handler.rs` 的 `kernel_trap`；`os/src/kernel/syscall/sys.rs` 的 `clock_settime`
- 现象：入口用 `csrrw` 把陷阱帧指针换进 `a0`，用户原来的 `a0` 留在 `sscratch`。保存完寄存器后没有把陷阱帧指针写回去，要到 `__restore` 才 `csrw sscratch`。这中间内核再发生异常，下一次 `trap_entry` 会把用户 `a0` 当成陷阱帧基址。`clock_settime(CLOCK_REALTIME, tp)` 用 `core::ptr::read(tp)` 读结构，不设 `sstatus.SUM`，也不检查 `tv_nsec`。S 态访问 U 页会缺页，接着走进 `kernel_trap` 的 `panic!("Kernel exception in S-Mode")`。`CLOCK_REALTIME` 的编号是 0，嵌套入口会向地址 0 存寄存器。传入内核地址时这次读会成功，读到的 16 字节被写进实钟偏移。
- 触发：`clock_settime(CLOCK_REALTIME, &ts)`，`ts` 是合法的用户指针；或把 `tp` 指到内核里的任意 16 字节。
- 修复：进入 C 处理函数之前把陷阱帧指针放回 `sscratch`。内核访问用户内存统一走会返回 `EFAULT` 的拷贝，并在拷贝窗口里做异常恢复。

### 用户缺页直接结束任务，已安装的 `SIGSEGV` 不会运行

- 严重度：致命
- 位置：`os/src/arch/riscv/trap/trap_handler.rs` 的 `user_trap` 默认分支；`os/src/mm/page_table/page_table_entry.rs` 的 `user_read` / `user_rw` / `user_rx`
- 现象：除了系统调用和中断，用户态异常都打印后 `terminate_task(128 + sig)`。指令、加载、存储缺页因此不会跑处理函数，也不会重试。非法指令是 `SIGILL`，缺页是 `SIGSEGV`，其余异常号一律 `SIGILL`，不对齐访问也不会变成 `SIGBUS`。另外这些构造函数只写 V/R/W/X/U，不置 A/D。Svade 机器在 A 为 0、或 store 时 D 为 0 时发缺页，处理函数既不置位也不返回原 PC，第一条用户指令就会把进程杀掉。默认 QEMU `rv64` 由硬件更新 A/D，空指针解引用仍然走这条直接退出的路径。
- 触发：装上 `SIGSEGV` 处理函数后解引用空指针。换 Svade CPU 后，任何用户程序的第一条访存也会被杀掉。
- 修复：PTE 有效但缺 A/D 时置位并原 PC 返回。其余按信号投递，不要直接退出。

### 切换和信号上下文都不保存浮点寄存器

- 严重度：高
- 位置：`os/src/arch/riscv/kernel/switch.S`；`os/src/arch/riscv/trap/trap_entry.S`
- 现象：`switch` 只保存 `ra`、`sp` 和 `s0`–`s11`。陷阱入口和恢复路径里没有 `fld` / `fsd`。两个都用过浮点的任务一切换，`f0`–`f31` 和 `fcsr` 就串在一起。信号处理返回时也没有一份可恢复的浮点上下文。
- 触发：两个用户任务都做浮点运算，中间发生时钟中断并切换。
- 修复：`FS` 不是 Off 时，在切换和进入信号帧时保存并恢复浮点状态。

### 内核线程的 `tp` 停在创建它的那颗 CPU 上

- 严重度：高
- 位置：`os/src/arch/riscv/trap/trap_frame.rs` 的 `set_kernel_trap_frame`；`os/src/arch/riscv/trap/trap_entry.S` 的 `__restore`；`os/src/kernel/cpu.rs` 的 `switch_task`
- 现象：创建内核线程时把 `x4_tp` 写成当时的 `current_cpu()`。返回用户或内核线程时，`__restore` 从陷阱帧偏移 32 把这个值装回 `tp`。迁移只更新陷阱帧里的 `cpu_ptr`。`trap_entry` 进内核时会暂时用 `cpu_ptr` 装对 `tp`，`sret` 时又装回创建者的 CPU 指针。`cpu_id()` 和每 CPU 运行队列都读 `tp`。
- 触发：在 CPU 0 上创建内核线程，调度器把它放到另一颗 CPU 上跑。
- 修复：切入内核线程时把 `x4_tp` 写成目标 CPU。内核态返回不要从陷阱帧恢复 `tp`。

### `ecall` 的返回地址在系统调用完成前就加了 4

- 严重度：中
- 位置：`os/src/arch/riscv/trap/trap_handler.rs` 的 `user_trap`；`os/src/ipc/signal.rs` 写明还没有 `SA_RESTART`
- 现象：进入 `dispatch_syscall` 之前就把 `sepc` 设成 `sepc + 4`。阻塞的 `read`、`poll`、`futex`、`nanosleep` 被信号打断时返回 `-EINTR`，没有路径把 `sepc` 减回 `ecall`。musl 给多数处理函数带 `SA_RESTART`，这些调用仍会把 `EINTR` 交给用户。
- 触发：处理函数带 `SA_RESTART` 时，阻塞的 `read` 或 `poll` 期间收到信号。
- 修复：需要重启时保留原来的 `sepc`。确定返回 `-EINTR` 或成功之后，再决定要不要前进 4 字节。

### ELF 的 `PT_LOAD` 把 `p_flags` 原样写成 Sv39 叶页

- 严重度：高
- 位置：`os/src/kernel/task/exec_loader.rs` 映射 `PT_LOAD` 的循环；`os/src/mm/memory_space/space/elf_loader.rs` 有同样的权限组合
- 现象：权限从 `VALID | USER` 起步，再按 `PF_R` / `PF_W` / `PF_X` 或上去，不补 R。Sv39 里 W=1 且 R=0 是保留编码。V=1 且 R/W/X 全 0 不是“不可访问的叶页”，而是下一级页表指针。随后按这个虚拟地址写入的文件内容会被 MMU 当成页表项。
- 触发：`PT_LOAD` 只有 `PF_W`，或 `p_flags == 0`。仓库里现成的 busybox、iperf3 是 `RE` 和 `RW`，不会走到这里。
- 修复：有 W 就带上 R。R/W/X 都没有时建成保留区，不要写 V=1 的叶 PTE。

### 两个 `PT_LOAD` 落在同一页时，整个 `exec` 失败

- 严重度：高
- 位置：`os/src/kernel/task/exec_loader.rs` 把每个 LOAD 收成整页后调用 `insert_framed_area`
- 现象：字节范围可以不相交，页范围仍然相交，例如对齐小于一页，或链接时没有把代码和数据拆到不同页。重叠时插入失败，`?` 让这次 exec 整个返回错误。段尾伸进已经留给栈的区间时也是同一次失败。
- 触发：两个 LOAD 的页范围相交，例如虚拟地址 `0x10000` 长度 `0x10`，和 `0x10100` 长度 `0x10`。已核对的仓库 ELF 只是页相邻，不会因此加载失败。
- 修复：按页合并已有 VMA，权限取并集，文件字节写入页内对应偏移。字节范围真的重叠再拒绝。

### 动态链接器的装载偏移会被夹成 0，重定位类型不认识就 `ENOEXEC`

- 严重度：高
- 位置：`os/src/kernel/task/exec_loader.rs` 计算 `load_bias` 和应用重定位；`os/src/arch/abi.rs` 的 `classify_relocation`
- 现象：解释器这条 `ET_DYN` 没有 `base_hint`。找到的空洞起点小于段的链接地址时，`saturating_sub` 得到 0，段被装回链接地址，而不是那个空洞，还可能和主程序或栈重叠。`AT_BASE` 也会变成 0。重定位只认类型 3（RELATIVE）和类型 2（64）。`R_RISCV_NONE`（0）以及其他类型让 `classify_relocation` 返回 `None`，整次 exec 变成无效 ELF。地址加法是 `load_bias + p_vaddr` 和 `p_vaddr + p_memsz`，release 下回绕后会在绕回去的低地址建映射。
- 触发：解释器最低虚拟地址高于空闲洞；或静态 PIE 的重定位里带类型 0。仓库里的 libc.so 最低虚拟地址是 0，这条饱和减法对它不触发。
- 修复：`checked_sub`，装不下就换地址或返回 `ENOEXEC`。`R_NONE` 跳过。地址加法溢出就拒绝这个 ELF。

### `AT_RANDOM` 是编译进内核的常量，`AT_EXECFN` 用的是 `argv[0]`

- 严重度：高
- 位置：`os/src/arch/riscv/kernel/task.rs` 构建 auxv 的那段；LoongArch 的用户栈使用同一组常量
- 现象：16 字节随机数固定是 `89 ab cd ef 01 23 45 67` 重复两遍。musl 拿 `AT_RANDOM` 的前 8 字节当 `__stack_chk_guard`，每个进程的 canary 都相同。`AT_EXECFN` 取 `arg_ptrs.last()`，压栈顺序使它等于 `argv[0]`，参数为空时是 0。`AT_HWCAP` 写成 0。`AT_PHDR`、`AT_ENTRY`、`AT_BASE`、`AT_CLKTCK` 的编号和取值与这里的加载结果一致。
- 触发：带 stack protector 的 musl 程序；`execve("/bin/busybox", ["sh", ...])` 后读 `AT_EXECFN`。
- 修复：用内核熵填这 16 字节。把 `execve` 的路径单独拷到栈上作为 `AT_EXECFN`。`AT_HWCAP` 置上这块机器实际有的扩展位。

## 挂载、proc 与其他文件系统

### 叠挂和符号链接会把挂载安到跟随之后的 dentry 上

- 严重度：高
- 位置：`os/src/vfs/mount.rs` 的 `MountTable::mount`；`os/src/vfs/path.rs` 的 `vfs_lookup` 和 `check_mount_point`
- 现象：挂载点不是 `/` 时，`set_mount` 打在 `vfs_lookup` 的结果上。查找会穿过已有挂载和最后一级符号链接。`check_mount_point` 只要 `get_mount()` 有值就返回那棵已挂上的根，不再看挂载栈顶。第二次挂到同一路径时，新根挂在旧根的 dentry 上，原来的挂载点仍指向旧根，路径查找看不到栈顶。符号链接也一样：表键是词法路径，指针打在跟随之后的目标目录上。`umount` 按表键清缓存里的 dentry，目标目录上的指针还在。
- 触发：对同一目录 `mount` 两次再打开它；或 `ln -s /etc /link` 之后把文件系统挂到 `/link`。
- 修复：`set_mount` 打在挂载点自己的 dentry 上。叠挂时替换这个指针，或让查找跟到栈顶。`umount` 清同一个 dentry。

### `umount` 不看占用，并且先把挂载从栈里拿掉再 sync

- 严重度：高
- 位置：`os/src/kernel/syscall/fs/mount_ops.rs` 的 `umount2`；`os/src/vfs/mount.rs` 的 `umount`
- 现象：`flags` 被丢掉，打开的文件、当前目录和子挂载都不返回 `EBUSY`。实现先 `pop` 挂载栈，再 `sync` 和 `umount`。sync 失败时函数返回，表里已经没有这项，dentry 上的挂载指针也还没改，后面无法再卸。成功路径按词法路径改缓存，不走 `remove_tree`，仍被引用的子路径缓存继续指向已卸载的文件系统。
- 触发：`cd /mnt && umount /mnt`；或 vfat 的 flush 在 umount 时失败。
- 修复：有引用、当前目录或子挂载时返回 `EBUSY`。先 sync，成功后再摘表并清掉这棵缓存；失败则把挂载推回栈顶。

### `find_mount` 把 `/mnt` 当成 `/mnt2` 的挂载点

- 严重度：高
- 位置：`os/src/vfs/mount.rs` 的 `find_mount`
- 现象：匹配条件是 `normalized_path.starts_with(mount_path)`，不要求下一个字符是 `/`。`"/mnt2/file".starts_with("/mnt")` 为真。路径查找的慢速路径另外要求 `mount_path == full_path`，打开文件不一定因此走错文件系统。直接使用 `find_mount` 结果的 `statfs` 会把 `/mnt` 的文件系统报给 `/mnt2`。
- 触发：挂上 `/mnt` 之后对 `/mnt2` 调用 `statfs`。
- 修复：路径相等，或挂载路径后面紧跟 `/`，才算命中。根路径单独比较。

### 同一块 VFAT 设备可以挂两次，两次的簇分配互不知道

- 严重度：高
- 位置：`os/src/fs/vfat/fs.rs` 的 `with_fs`；挂载路径不拒绝已挂上的设备
- 现象：每次操作都在自己的 `op_lock` 下新建一个 `fatfs::FileSystem`。两份 `VfatState` 共用同一个块设备，空闲簇和 FAT 链各算各的，写回会互相覆盖。上一节又允许旧挂载上的文件描述符在 umount 之后继续写。
- 触发：把同一磁盘挂到 `/mnt` 和 `/mnt2`，两边同时创建文件。
- 修复：按设备拒绝第二次挂载，除非是明确的绑定挂载。

### `/proc` 的数字目录按 tid 打开，子文件还会被缓存钉住

- 严重度：高
- 位置：`os/src/fs/proc/inode.rs` 的 `lookup`、`create_process_dir`、`cacheable`；`os/src/vfs/path.rs` 在 `cacheable()` 为真时把 dentry 放进全局缓存
- 现象：`name.parse::<u32>()` 接受 `01` 和 `+1`，然后 `get_task` 按 tid 查。`/proc/01` 打开的是 tid 1，目录列举里却只有不带前导零的进程号。非 leader 线程的 tid 同样能打开。进程目录自己 `cacheable()` 为假，但 `status` 等子节点不是 `PidDir`，默认可以缓存，并在创建时抓住当时的任务。绝对路径查找先查 dentry 缓存。缓存还在时，这个 tid 再被用到，打开同一路径仍返回旧任务。
- 触发：打开 `/proc/01`；或打开 `/proc/<tid>/status` 不关闭，等该 tid 再次分配后打开同一路径。
- 修复：只接受没有前导零、没有正号的十进制，并且必须是进程 pid。子文件不要缓存，或在任务退出时作废。

### simple_fs 按镜像头里的长度分配内存，inode 号会和活着的文件撞车

- 严重度：高
- 位置：`os/src/fs/simple_fs.rs` 读取镜像头，以及 `create`
- 现象：`name_len` 和 `data_len` 没有和镜像剩余长度比较，就按声明的字节数 `vec!`。损坏的头可以在读盘失败之前把分配器打满。`create` 用 `children.len() + 2` 当 inode 号：建两个文件再删掉先建的那个，下次创建会得到还活着的那个文件的号。
- 触发：镜像头里把 `data_len` 写成很大的值；或在同一目录创建、删除、再创建后比较 `st_ino`。
- 修复：分配前检查剩余长度和上限。inode 号用单调计数器。

### VFAT 上只改大小写会返回成功，目标已存在时先把目标删掉

- 严重度：中
- 位置：`os/src/fs/vfat/inode.rs` 的 rename
- 现象：同一目录里新旧名字只有大小写不同时直接 `Ok(())`，盘上的长文件名不会改。目标已经存在时，先 `remove` 再继续改名。后面的步骤失败的话，目标已经不在了。
- 触发：在 vfat 上 `rename("Foo", "foo")` 后 `readdir`；或把文件改成一个已存在的名字，而随后的 fatfs rename 失败。
- 修复：仅大小写变化也要走 fatfs 的 rename。替换目标放到新名字创建成功之后。

## 块设备、页缓存与控制台

### virtio-blk 在睡眠锁里等请求完成，中断处理又来拿同一把锁

- 严重度：致命
- 位置：`os/src/device/block/virtio_blk.rs` 的 `read_blocks` 和 `try_handle_interrupt`；`os/src/sync/mutex.rs` 的 `Mutex::lock`
- 现象：读写整段持有 `Mutex<VirtIOBlk<...>>`。这把锁拿不到时会 `sleep` 当前任务并 `yield_task`。设备中断调用 `try_handle_interrupt`，里面再次 `lock()`。中断若发生在同一次读写还没放开锁的时候，被打断的任务自己睡在自己持有的锁上，`read_blocks` 不会返回，队列里的描述符也不会收割。
- 触发：开中断之后的 virtio-blk 读写。QEMU 常在 notify 的 MMIO 返回时就把中断置上。
- 修复：提交和收割用关中断的自旋锁。中断里只做 `try_lock` 来应答，拿不到就返回。

### ext4 页缓存先填页再插入，短页会被当成文件结束

- 严重度：高
- 位置：`os/src/vfs/page_cache.rs` 的 `get_or_insert_clean_page`、`read_hit`；`os/src/fs/ext4/inode.rs` 的 `read_at`、`write_at`
- 现象：填页回调里持文件系统锁读盘，回调返回后锁已经放下，然后才把页插进缓存。写路径在写入成功后 `refresh_clean_range`，那只是把已经在表里的页删掉。读端已经拷好旧数据、但还没插入时，写端的删除打空，读端随后把旧页插回去。`read_hit` 在页内偏移不小于已缓存长度时返回 `Some(0)`。`read_at` 看到 0 就 `break`，不会按新的文件大小重填这一页。
- 触发：一个任务读未命中，另一个任务在填页返回和插入之间写同一范围。或者这一页当初只缓存了文件尾部的短内容，文件后来变长，再从页内靠后的偏移读取。
- 修复：插入和失效放在同一把锁里，插入前发现这一页已被写过就丢掉这次填页。页内偏移越过已缓存长度时按未命中重填。

### ext4 读块失败时返回一块全零

- 严重度：高
- 位置：`os/src/fs/ext4/adpaters.rs` 的 `read_offset`
- 现象：`read_blocks` 返回 false 时，函数返回 `vec![0u8; block_size]`。上层分不清这是读失败还是盘上本来就是零。ext4 的部分块写入会先读再整块写回，失败的那一块其余字节会被写成 0。
- 触发：底层块读失败时的 ext4 读改写。
- 修复：读失败向调用方返回错误，不要交出全零缓冲。

### 根文件系统镜像用文件内容时间决定要不要重建，删除文件不会让它失效

- 严重度：中
- 位置：`os/build.rs` 的 `get_latest_mtime`
- 现象：`Path::is_file` 跟随符号链接，比较的是目标文件的修改时间。目录分支只看子项，不看目录自己的 mtime。从 `data/` 删掉一个更旧的文件后，剩下的文件如果都比镜像旧，镜像不会重建，里面仍是已删除的文件。改符号链接本身也不会更新这个时间。
- 触发：删掉 `data/risc-v_musl` 里一个旧文件后增量编译。
- 修复：用 `symlink_metadata`，并把目录自身的修改时间算进去。

### 串口读行在回车处停下，换行留在后面

- 严重度：中
- 位置：`os/src/device/console/uart_console.rs` 的 `read_char` 和 `read_line`
- 现象：读到 `\r` 或 `\n` 都结束当前行。终端通常发 `\r\n`，`\n` 留在接收缓冲里，下一次 `read_line` 立刻得到空行。回显把刚读到的字节原样写回，退格也推进调用方的字符串，行缓冲不会删字符。
- 触发：串口输入 `\r\n` 后连续读两行。
- 修复：把 `\r\n` 收成一个换行。退格从行缓冲删掉前一个字符，回显不要把退格字节本身推进字符串。

### Goldfish RTC 的高 32 位和低 32 位可能不是同一次采样

- 严重度：中
- 位置：`os/src/device/rtc/rtc_goldfish.rs` 的 `read_goldfish_epoch`
- 现象：先读 `TIME_LOW` 再读 `TIME_HIGH`，两次 MMIO 之间没有锁。读低位会锁存高位，另一个核再读低位会重新锁存。本核随后读到的高位可能属于另一次采样。跨过 2^32 纳秒边界时，拼出来的时间会差大约 4 秒。
- 触发：两个任务同时读 RTC。
- 修复：用一把锁包住两次读。读完后再读一次低位，不一致就重试。

### RamDisk 的块号乘法回绕后会读写别的块

- 严重度：中
- 位置：`os/src/device/block/ram_disk.rs` 的 `read_block` 和 `write_block`
- 现象：偏移是 `block_id * block_size`。release 下乘法回绕，回绕后的偏移若还落在盘内，函数返回成功，读写的是另一块。debug 配置会在内核里溢出 panic。
- 触发：块号大于 `usize::MAX / block_size`，例如损坏的文件系统给出的块号。
- 修复：用 `checked_mul`。溢出或越过盘尾都返回失败。

## 终端、进程组与调度

### 标准输入的回显关不掉，默认还会回显两次

- 严重度：高
- 位置：`os/src/device/console/uart_console.rs` 的 `read_char`；`os/src/vfs/impls/stdio_file.rs` 的 `StdinFile::read`
- 现象：串口取出一个字节后无条件写回去。fd 0 在 `ECHO` 打开时再 `console_putchar` 一次。`TCSETS` 清掉 `ECHO` 只关掉第二次。
- 触发：读标准输入；或 `stty -echo` 之后再读。
- 修复：串口层不要回显。只在行规程里、并且 `ECHO` 置位时输出处理后的字符。

### 控制台锁拿不到时，读标准输入变成文件结束

- 严重度：高
- 位置：`os/src/console.rs` 的 `getchar`；`os/src/vfs/impls/stdio_file.rs` 的 `StdinFile::read`
- 现象：`getchar` 对 `CONSOLE_LOCK` 使用 `try_lock`，失败就返回 `None`。标准输入把 `None` 当成没有更多输入并结束这次 `read`。一个字节都没读到时返回值是 0。
- 触发：一颗 CPU 正持有控制台锁输出，另一颗 CPU 对 fd 0 调用 `read`。
- 修复：没有字节时阻塞或返回 `EAGAIN`。锁竞争要重试，不要把它变成 EOF。

### 行规程不处理退格、Ctrl-D 和 Ctrl-C

- 严重度：高
- 位置：`os/src/vfs/impls/stdio_file.rs` 的 `StdinFile::read`；`os/src/vfs/impls/char_dev_file.rs` 的 `CharDeviceFile::read`
- 现象：默认 `c_lflag` 含 `ISIG|ICANON|ECHO`，`c_cc` 里有 `VINTR`、`VERASE`、`VEOF`。读路径只做 CR/NL 映射，并在规范模式下看到换行就返回。退格、Ctrl-D、Ctrl-C 都进用户缓冲。非规范模式读到 1 字节就返回，不看 `VMIN` / `VTIME`。字符设备的非阻塞规范读有字节就返回，不等整行。
- 触发：默认终端上按退格或 Ctrl-D；把 `VMIN` 设成 8 后做非规范读。
- 修复：规范模式在入队时处理编辑字符和 `ISIG`，只有完整行才对 `read` 可见。非规范模式按 `VMIN` 和 `VTIME` 决定阻塞和返回长度。

### 对字符设备 `read(fd, buf, 0)` 会写下标 0

- 严重度：致命
- 位置：`os/src/vfs/impls/char_dev_file.rs` 的 `CharDeviceFile::read`
- 现象：系统调用按 `count` 分配内核缓冲。长度为 0 时仍然进入驱动读。阻塞路径等到一个没有被 `IGNCR` 丢掉的字节后执行 `buf[count] = mapped`，此时 `count` 是 0。非阻塞路径在硬件里已经有字节时同样写 `buf[0]`。空切片下标在调试配置里会让内核 panic。
- 触发：打开 `/dev/ttyS0` 或 `/dev/console`，`read(fd, buf, 0)`，阻塞时再输入一个字符。
- 修复：`buf` 为空时直接返回 0，不要读硬件。

### 阻塞读终端在内核里空转，信号送不进去

- 严重度：高
- 位置：`os/src/vfs/impls/char_dev_file.rs` 的 `CharDeviceFile::read`
- 现象：没有字节时 `spin_loop` 后继续转，不进入可中断睡眠。信号要等这次系统调用返回、陷阱帧回到用户态才会投递。
- 触发：对 `/dev/console` 阻塞 `read`，另一个任务向该进程发 `SIGKILL` 或普通信号。
- 修复：放进等待队列并做可中断睡眠。被信号叫醒且还没读到数据时返回 `EINTR`。

### `poll` / `select` 认为终端永远可读

- 严重度：高
- 位置：`os/src/kernel/syscall/io.rs` 的 `file_read_ready`；`StdinFile::readable` 固定返回 true
- 现象：只有管道会看缓冲里有没有数据。标准输入和字符设备只要打开方式允许读，就报告可读。输入队列里没有字节时也是这样。
- 触发：对 fd 0 或 `/dev/console` 做 `poll(POLLIN)`，此时没有输入。
- 修复：终端有自己的输入队列。`poll` 只在队列满足规范模式或 `VMIN` 时返回可读。

### `TIOCGPGRP` / `TIOCSPGRP` 读写的是调用进程自己的进程组

- 严重度：高
- 位置：`os/src/kernel/syscall/ioctl.rs` 的 `handle_tiocgpgrp` 和 `handle_tiocspgrp`
- 现象：这两个请求不看 fd 是不是终端。`TIOCGPGRP` 把当前任务的 `pgid` 写给用户。`TIOCSPGRP` 把读到的整数写回 `task.pgid`。终端对象上没有前台进程组。`TIOCSCTTY` 直接返回 0，也不记录控制终端。
- 触发：对普通文件或管道调用 `TIOCGPGRP`；shell 用 `TIOCSPGRP` 把前台交给子进程组。
- 修复：前台组记在这个终端上。先确认 fd 是该终端，再按会话检查。不要改 `Task::pgid`。

### 标准输出不处理 `ONLCR`，每打开一次串口就有一份 termios

- 严重度：中
- 位置：`os/src/vfs/impls/stdio_file.rs` 的 `STDIO_TERMIOS` 和 `StdoutFile::write`；`os/src/vfs/impls/char_dev_file.rs` 的 `termios` 字段
- 现象：fd 0/1/2 共用一份 `STDIO_TERMIOS`，但标准输出原样写出，不看 `OPOST|ONLCR`。每次打开 `/dev/ttyS0` 又有自己的 `termios` 和 `winsize`。`TIOCSWINSZ` 只改这一份，不发 `SIGWINCH`。字符设备自己的写路径会做 `ONLCR`。
- 触发：fd 1 写 `"\n"`；在标准输入上 `TCSETS` 之后再打开 `/dev/console` 做 `TCGETS`。
- 修复：termios 和窗口大小放在终端设备上，所有打开共享。所有写路径走 `OPOST`。窗口尺寸变化时给该终端的前台组发 `SIGWINCH`。

### `FIONREAD` 用文件大小减当前偏移

- 严重度：中
- 位置：`os/src/kernel/syscall/ioctl.rs` 的 `handle_fionread`
- 现象：`metadata()` 成功时把 `size - offset` 转成 `i32`。管道的 `size` 固定是 0，套接字的 `metadata()` 返回 `NotSupported`，两种设备都写成 0。普通文件大于 2GiB 时这个差值会被截成 `i32`。
- 触发：往管道写入后再对读端 `ioctl(FIONREAD)`。
- 修复：管道、套接字和终端返回已经排队、尚未读走的字节数。普通文件的剩余长度不要截断成 32 位。

### `setpgid` 把进程组首领当成会话首领，`setsid` 只改当前线程

- 严重度：高
- 位置：`os/src/kernel/syscall/task/process_ops.rs` 的 `set_pgid`；`os/src/kernel/syscall/task/session_ops.rs` 的 `setsid`
- 现象：`set_pgid` 用调用线程的 `tid` 当进程号。`pgid == tid` 就返回 `EPERM`。子进程第一次 `setpgid(0, 0)` 会把 `pgid` 设成自己的 tid，之后父进程或子进程再 `setpgid(pid, pid)` 被拒绝。新的 pgid 不要求等于目标 pid，也不要求是同一会话里已经存在的组。`setsid` 在 `pid == pgid` 时失败，这和“已经是进程组首领”一致；成功时只写当前这个 `Task` 的 `pgid`，同进程的其他线程保持原值。没有单独的会话号。
- 触发：shell 在父子两边都调用 `setpgid(pid, pid)`。或多线程进程里一个非 leader 线程调用 `setsid`，再用 leader 的 pid 去 `getpgid`。
- 修复：给进程保存 sid。`setpgid` 和 `setsid` 修改线程组里的每一个线程。只有 `sid == pid` 才拒绝 `setpgid`。新组必须是目标自己的 pid，或调用者会话中已有的 pgid。

### `sched_setscheduler` 改完策略，任务仍留在原来的队列

- 严重度：高
- 位置：`os/src/kernel/syscall/task/sched_ops.rs` 的 `set_scheduler_common`；`os/src/kernel/scheduler/rr_scheduler.rs` 的 `enqueue_task`
- 现象：系统调用只写 `sched_policy` 和 `sched_priority`。队列是在入队时按当时的策略选择的，已经排在 `fair_queue` 或 `rt_queue` 里的任务不会被摘下来重插。从 `SCHED_FIFO` 改回 `SCHED_NORMAL` 后，任务仍留在实时队列里，只要这队列非空，普通任务就会被它抢占。
- 触发：线程 B 已在运行队列中，线程 A 对 B 调用 `sched_setscheduler` 改成 `SCHED_FIFO`，或把一个正在排队的 FIFO 线程改回 `SCHED_NORMAL`。
- 修复：目标若已在某个运行队列中，按新策略移队，并让它所在的 CPU 重新调度。

### 三种间隔定时器都按开机后的单调时间计时

- 严重度：高
- 位置：`os/src/kernel/syscall/task/time_ops.rs` 的 `setitimer`；`os/src/uapi/time.rs` 的 `timeval::into_freq` 和 `to_timespec`
- 现象：`ITIMER_REAL`、`ITIMER_VIRTUAL`、`ITIMER_PROF` 都用 `get_time() + it_value.into_freq()` 放进同一条队列。进程睡着时虚钟和性能钟也会到期。负的 `tv_sec` 经 `as u128` 变成很大的正延迟。`tv_usec * 1000` 在调试配置里可能溢出 panic。旧定时器在读入新值之前就被删掉，新值拷贝失败时旧定时器已经没了。
- 触发：`setitimer(ITIMER_VIRTUAL, 1 秒)` 后进程阻塞；或 `it_value.tv_sec = -1`。
- 修复：实钟用墙上时间的绝对到期点。虚钟和性能钟只在对应的 CPU 时间里递减。先校验并复制用户结构，再替换旧表项。

### `select` / `pselect` 始终按 1024 位拷贝 `fd_set`

- 严重度：高
- 位置：`os/src/uapi/select.rs` 的 `FdSet`；`os/src/kernel/syscall/io.rs` 的 `select_common` 和 `write_select_fd_sets`
- 现象：`FD_SETSIZE` 是 1024，riscv64 上这个结构是 128 字节。指针非空时，不论 `nfds` 是 0 还是 1，都整份读入，返回或超时再整份写回。循环本身只检查 `0..nfds`。用户只分配了覆盖 `nfds` 的那几个 `long` 时，写回会覆盖后面的栈或缓冲。
- 触发：`pselect(1, &one_long, NULL, NULL, ...)`，其中 `one_long` 只有 8 字节。
- 修复：拷贝长度按 `nfds` 向上取整到 `sizeof(long)`。只写回这一段。`nfds == 0` 时不要碰用户指针。

## 共享内存与映射

### 对 SysV 共享段 `mprotect(PROT_NONE)` 会丢掉整段 VMA，页表项还在

- 严重度：致命
- 位置：`os/src/mm/memory_space/space/mmap_ops.rs` 的 `MemorySpace::mprotect`；`os/src/mm/memory_space/mapping_area/split_ops.rs` 里 `MapType::Shared` 的分支；`os/src/kernel/syscall/ipc.rs` 的 `shmdt`
- 现象：处理前先把 `MappingArea` 从 `areas` 摘掉。共享段在新权限没有 R/W/X 时直接返回错误，不改页表，调用方也不会把这块 area 插回去。系统调用把所有错误都变成 `-ENOMEM`。这段地址从此不在 VMA 列表里，PTE 仍指向共享段的物理页。`munmap` 找不到 VMA 时当作成功。`shmdt` 因此只减 `nattch`，不拆 PTE。`IPC_RMID` 且 attach 计数到 0 之后，这些帧可以重新分给别的映射。只改其中一页也会把整段 VMA 丢掉，因为出错发生在拆分结果写回之前。普通帧映射的 `PROT_NONE` 是另一条路径，那条会解除映射并释放页内容。
- 触发：`shmat` 之后 `mprotect(addr, len, PROT_NONE)`，再 `shmctl(IPC_RMID)` 和 `shmdt`，然后访问原地址。
- 修复：`PROT_NONE` 只清掉用户访问权限，保留 VMA 和共享段引用。出错时把原 area 插回去。`shmdt` 按附件记录拆 PTE。

### `munmap` 和 `SHM_REMAP` 不维护 attach 记录

- 严重度：高
- 位置：`os/src/kernel/syscall/mm.rs` 的 `munmap` 和 `mmap` 的 `MAP_FIXED`；`os/src/kernel/syscall/ipc.rs` 的 `shmat`、`shmdt`
- 现象：解除映射只改 VMA 和页表，不碰 `shm_attachments`，也不减 `nattch`。之后 `shmdt` 或进程退出仍按当初的整段地址再 `munmap` 一次，会把后来放在同一地址上的映射一起拆掉。`SHM_REMAP` 只按新起点这个键删掉一条附件，然后按新段的长度 `munmap`。旧段更长时，尾部仍映射着旧帧，`nattch` 已经减完。旧段起点不同、只重叠一部分时，旧附件还在。占用地址上不带 `SHM_REMAP` 的 `shmat` 把 `AlreadyMapped` 一律返回 `-ENOMEM`。`shmaddr == NULL` 时，`find_free_region` 和真正插入之间会放下地址空间锁。插入失败时，已经卸掉的旧附件会被 detach，映射不恢复。
- 触发：两页的 `shmat` 之后 `munmap` 第一页，再在这一页上 `mmap`，然后 `shmdt(原地址)`。或者对一个两页段的起始地址，带 `SHM_REMAP` 再 `shmat` 一个一页的段。
- 修复：`munmap`、`mmap(MAP_FIXED)` 和 `SHM_REMAP` 按重叠区间拆分附件，只对真正消失的 attach 减 `nattch`。`shmdt` 只解除仍属于该附件的页。地址已被占用时返回 `EINVAL`。选地址和插入放在同一把地址空间锁里。

### 只读 attach 可以被 `mprotect` 改成可写

- 严重度：高
- 位置：`os/src/kernel/syscall/ipc.rs` 的 `shmat`；`split_ops.rs` 里共享段的 `update_flags`
- 现象：`SHM_RDONLY` 只在当时不把 PTE 标成可写。之后 `mprotect(PROT_READ|PROT_WRITE)` 对共享段直接改标志，不看 attach 时的权限，也不看段的 mode。`SHM_EXEC` 同样不检查 mode 里的执行位，访问检查只用读/写那三位。
- 触发：`shmat(id, NULL, SHM_RDONLY)` 然后 `mprotect(p, len, PROT_READ|PROT_WRITE)`。写入对其他 attach 可见。
- 修复：VMA 记下这次 attach 允许的最大权限。`mprotect` 不得超过它。`SHM_EXEC` 再按 mode 的执行位检查。

### `MAP_SHARED` 建的是私有帧，`fork` 再拷一份

- 严重度：高
- 位置：`os/src/kernel/syscall/mm.rs` 的 `mmap`；`os/src/mm/memory_space/space/address_space.rs` 的 `clone_for_fork`
- 现象：`MAP_SHARED` 和 `MAP_PRIVATE` 都会 `insert_framed_area`，文件内容各自 `load_from_file` 进新帧，不进同一套页缓存。`fork` 对所有 `MapType::Framed` 调用 `clone_with_data`。只有 SysV 的 `MapType::Shared` 会在子进程页表里再映射同一批物理页。匿名 `MAP_SHARED` 在 `fork` 之后也变成父子各一份。
- 触发：两个进程 `mmap` 同一文件且带 `MAP_SHARED`，一方写入映射，另一方仍看到加载时的旧数据。或者 `mmap(MAP_SHARED|MAP_ANONYMOUS)` 后 `fork`，父进程的写入子进程看不见。
- 修复：文件共享映射走同一套页缓存。匿名共享映射使用可共享的帧，并在 VMA 上记下共享属性。`fork` 对这两种映射只复制 VMA、再映射原物理页。

### `clone` 失败之后，多出来的 `nattch` 没有人减

- 严重度：高
- 位置：`os/src/kernel/syscall/task/clone_ops.rs`；`os/src/kernel/task/task_struct.rs` 的 `Drop for Task`
- 现象：不带 `CLONE_THREAD` 时，先对每条附件 `mark_attached`，然后才写 `CLONE_CHILD_SETTID`。用户地址写失败就返回 `-EFAULT`。局部的子任务被丢掉，`Task` 的 `Drop` 只打日志，不调用 `detach_all_shm`。这次增加的 `nattch` 没有对应进程。
- 触发：进程已经 `shmat`，再 `clone` 并设置 `CLONE_CHILD_SETTID`，`ctid` 指向不可写地址。之后父进程 `shmdt` 并 `IPC_RMID`，段仍留在 registry 里。
- 修复：子任务一旦增加了 `nattch`，任何失败返回前都按它的附件表做一次 detach。或者把 `mark_attached` 挪到子任务已经交给调度器之后。

### `mlock` / `mlockall` 按 64KiB 拒绝，root 也过不了

- 严重度：高
- 位置：`os/src/kernel/syscall/mm.rs` 的 `mlock`、`mlockall`、`memlock_limit_allows`；`MEMLOCK_DEFAULT_LIMIT` 是 64KiB；用户栈 VMA 是 4MiB
- 现象：没有换出，锁本身可以不改页表，但限额判断是错的。单次 `mlock` 只拿这次的 `len` 和 `rlim_cur` 比，不累计，失败返回 `-EAGAIN`。`mlockall(MCL_CURRENT)` 把所有 VMA 的字节数加起来再和 64KiB 比，栈本身就已经超过。全程不看 `CAP_IPC_LOCK`。当前 root 凭证的能力集是满的。反过来，多次各 40KiB 的 `mlock` 都会成功。
- 触发：已经加载的用户进程调用 `mlockall(MCL_CURRENT)`，或 `mlock` 一段超过 64KiB 的已映射内存。
- 修复：有 `CAP_IPC_LOCK` 则跳过软限制。否则按页对齐后累计进已锁长度，超限返回 `ENOMEM`。`munlock` 再减回去。

### `shmget` 用 9 位权限和 3 位 mode 做相等比较

- 严重度：中
- 位置：`os/src/ipc/shared_memory.rs` 的 `get_or_create` 和 `shm_check_mode_access`
- 现象：已存在的 key 把 `shmflg & 0o666` 整段当作请求权限，再要求它是当前类别那 3 位的子集。`0666` 的请求值是 `0o666`，属主可用位最多是 7，比较永远失败。`CAP_IPC_OWNER` 在比较前直接放行。当前 root 凭证有这个能力，所以默认进程走不到失败分支。`shmat` 的请求值只有 `0o4` 或 `0o6`，不受这个错误影响。
- 触发：有效能力集不含 `CAP_IPC_OWNER` 时，对已存在的段执行 `shmget(key, size, 0666)`。
- 修复：先把请求折成 3 位，再和对应类别的 3 位比较。

### `madvise` 对未映射区间返回成功

- 严重度：中
- 位置：`os/src/kernel/syscall/mm.rs` 的 `madvise`
- 现象：只检查长度、页对齐、加法和 advice 编号。认识的 advice 一律返回 0，不看这段地址是不是已经映射。advice 本身没有页面效果。
- 触发：`madvise` 一段没有映射的页，或区间中间有空洞。
- 修复：有空洞或地址不在本进程地址空间时返回 `ENOMEM`。

## LoongArch

加载器、auxv 常量和页权限与 RISC-V 共用，这一轮没有另写专有条目。下面四条只在 LoongArch 的入口或参数顺序上出现。

### `clone` 的第 4、5 个参数装反了

- 严重度：致命
- 位置：`os/src/kernel/syscall/mod.rs` 的 `sys_clone`；`os/src/kernel/syscall/task/clone_ops.rs` 的 `clone`；`os/src/arch/loongarch/trap/trap_frame.rs` 的 `set_tls` 写 `regs[2]`
- 现象：系统调用在所有架构上都按 RISC-V 取参：`a3` 当 TLS，`a4` 当 `child_tid`。`CLONE_SETTLS` 把 `a3` 写入 `$tp`。`CLONE_CHILD_SETTID` 和 `CLONE_CHILD_CLEARTID` 把 `a4` 当成用户态 tid 指针。musl 的 `src/thread/loongarch64/clone.s` 和 glibc 的 `sysdeps/unix/sysv/linux/loongarch/clone.S` 都是 `a3 = ctid`、`a4 = tls`，系统调用号 220。子线程的 `$tp` 指向 tid 字，tid 写到 TLS 地址上。地址不可写时父调用得到 `EFAULT`，线程没有创建出来。
- 触发：LoongArch 上 `pthread_create`，或任何带 `CLONE_SETTLS | CLONE_CHILD_SETTID` 的 `clone`。不带这两个标志的 `fork` 不受影响。RISC-V 的参数顺序和当前绑定一致。
- 修复：LoongArch 按 `(flags, stack, ptid, ctid, tls)` 绑定。

### 用户态异常把整个内核 panic 掉

- 严重度：致命
- 位置：`os/src/arch/loongarch/trap/trap_handler.rs` 的 `user_trap`、`user_panic`
- 现象：在 IS 位全 0、进入同步异常分支时，`ecode` 不是 `0xb`（syscall）就打印寄存器后 `panic!`。RISC-V 同一类用户异常只结束当前任务。这里的特权级已经是用户态；IS 非 0 时还会先遇到下一条记录的误判。
- 触发：用户程序解引用空指针、执行非法指令，或写只读页。
- 修复：按 `ecode` 结束当前任务，或投递 `SIGSEGV`、`SIGILL`、`SIGBUS`。不要停掉内核。

### 用 `ESTAT.IS` 判断这次陷阱是不是中断

- 严重度：高
- 位置：`os/src/arch/loongarch/trap/trap_handler.rs` 的 `user_trap`、`kernel_trap`、`handle_interrupt`。掩码 `CSR_ESTAT_IS_MASK`（`0x1fff`）在 `os/src/arch/loongarch/constant.rs`
- 现象：`estat & 0x1fff != 0` 就当作中断，调用 `handle_interrupt` 后直接返回。该函数只确认定时器位（bit 11）并清对应的定时器中断。`ecode` 在 `[21:16]`，要等 IS 全 0 才看。中断使能只打开了定时器那一位。`ESTAT.IS` 表示各中断线是否挂起，和这次例外的原因无关。被屏蔽的挂起位仍可以是 1，同时 `ecode` 是 syscall 或页例外。误判之后不推进 `ERA`，也不处理这次例外。挂起位清不掉时，同一条指令会反复进入陷阱。
- 触发：syscall 或页例外发生时，任意没有被 `handle_interrupt` 清掉的 IS 位已经为 1。IS 全 0 时 syscall 仍能返回。
- 修复：只在 `Ecode == 0` 时走中断。同步例外即使 IS 非 0 也按 `Ecode` 处理。

### 信号帧里的通用寄存器错一格

- 严重度：高
- 位置：`os/src/arch/loongarch/trap/trap_frame.rs` 的 `to_mcontext`、`restore_from_mcontext`；`os/src/uapi/signal.rs` 的 `UContextT`、`MContextT`。信号投递把这份结构写到用户栈，`rt_sigreturn` 按同一布局读回
- 现象：`gregs[0]` 写成 `ERA`，`gregs[i] = regs[i]`（`i = 1..31`），`$r0` 没有槽。浮点紧跟在 `fpregs[0..31]`，`fpregs[32]` 是 `fcsr`，`fpregs[33]` 是 `fcc`。Linux LoongArch 的 `sigcontext` 是单独的 `sc_pc`，然后 `sc_regs[32]`（下标 n 就是 `$rn`），再是 `sc_flags` 和对齐的扩展上下文。按那个布局读，`$r1` 出现在 `sc_regs[0]`，`$r31` 出现在 `sc_regs[30]`，`sc_regs[31]` 则读到本实现的 `fpregs[0]`。Linux LoongArch 的 `ucontext` 同样保留 128 字节信号掩码区域，本实现的 `1024/8 - 8` 字节预留区不能单独算作错误。内核自己写、自己读时，不修改寄存器的处理函数还能返回，但用户态按 Linux 布局读 `ra`、`sp`、`a0` 会读错。用户缺页目前会先被上面的 `panic` 截住；`kill` 或定时器信号可以走到这里。依据：[Linux LoongArch sigcontext][verify-la-sigcontext]、[ucontext][verify-la-ucontext]。
- 触发：装上处理函数后收到信号，处理函数或取消点读取 `uc_mcontext`。
- 修复：定义 LoongArch 专用机器上下文，按 `sc_pc`、`sc_regs[32]`、`sc_flags` 和带魔数的浮点扩展上下文填写，`$rn` 放进 `sc_regs[n]`。同步修改投递和 `rt_sigreturn` 的布局、大小与对齐；保留 ABI 要求的信号掩码预留区。

## 复核后撤回的条目

### UDP 的 `connect` 和 `sendto` 改写已经绑定的本地地址（原触发不成立）

- 状态：2026-11-04 撤回原「高」严重度的用户可见缺陷结论，不是已修复；内核代码未修改。
- 原断言：`bind(192.168.1.100:port)` 后调用 `connect` 或 `sendto`，再 `getsockname` 会看到地址变成 `0.0.0.0` 或 `127.0.0.1`。
- 源码核对：`os/src/kernel/syscall/network/mod.rs` 的 `ensure_udp_bound_for_peer` 确实覆盖 `SocketFile.local_endpoint`，但保留原端口。`os/src/net/stack/mod.rs` 的 `udp_attach_fd_to_port` 命中已有 `udp_ports[port]` 时直接复用原句柄，根本不执行新的 `bind`。`socket_local_endpoint` 读取这个 smoltcp UDP socket 的 `endpoint()`；`addr_ops.rs::getsockname` 优先采用该结果，只有返回 `None` 才回退到 `SocketFile`。已绑定 UDP 不走该回退。
- 隔离验证：从当前源码抽取上述准备、挂接、查询函数及 `SocketFile` 访问器，使用真实 smoltcp 0.12.0，在宿主机以标准互斥锁和最小 File/句柄注册桩运行；两组用例均断言准备前后句柄相同、协议栈地址不变，且 `send_slice` 入队后仍保持原绑定。未执行完整系统调用、网卡收发或 QEMU，不能当作整机复现。

| 初始绑定 | 对端 | 准备后的 `SocketFile` 字段 | 查询函数返回的协议栈端点 |
| --- | --- | --- | --- |
| `192.168.1.100:51000` | `192.168.1.1:9000` | `0.0.0.0:51000` | `192.168.1.100:51000` |
| `192.168.1.100:51001` | `127.0.0.1:9000` | `127.0.0.1:51001` | `192.168.1.100:51001` |

- 结论：原来的 `getsockname` 触发链不成立，不能只凭内部字段被覆盖推断绑定已经改变。发送路径使用复用的 smoltcp 句柄；当前接收分发读取 `SocketFile.local_endpoint` 时只比较端口，也不能据此证明地址覆盖改变了收包行为。
- 后续：两份端点记录不一致仍是维护风险，但尚未确认独立的用户可见后果，不给它保留原严重度或错误修复前提。同端口的多个 socket 复用同一句柄、忽略后来的绑定地址，已在「同一端口可以绑多次，后一次的地址被丢掉」单独记录，不重复计数。

[verify-linux-unix]: https://github.com/torvalds/linux/blob/v6.12/net/unix/af_unix.c
[verify-smoltcp-tcp]: https://github.com/smoltcp-rs/smoltcp/blob/v0.12.0/src/socket/tcp.rs
[verify-la-sigcontext]: https://github.com/torvalds/linux/blob/v6.12/arch/loongarch/include/uapi/asm/sigcontext.h
[verify-la-ucontext]: https://github.com/torvalds/linux/blob/v6.12/arch/loongarch/include/uapi/asm/ucontext.h

## 尚未覆盖

下面这些区域还没有逐项核对完，不能从“没写在上面”推出没有问题：

- 消息队列的 `send` / `recv` 在放下状态锁之后才睡眠。当前没有 `msgsnd` / `msgrcv` 系统调用，用户程序到不了。System V 信号量和 `msync` 也没有系统调用入口
- tmpfs 与页缓存写回、ext4 的写路径和 `rename`、`exec` 的参数与 shebang、上面三条以外的 futex 操作、凭证与能力、virtio-gpu、virtio-input、virtio-console、IPI 与工作队列。上一轮派出的阅读在会话中断时被取消，没有报告。`setuid` / `setgid` 对非零 ID 返回 `EPERM` 是单 root 设计，不要记成缺陷
- LoongArch 的 TLB refill 入口，以及 `clone` 以外的系统调用号是否还有架构差异。加载器、auxv 常量和页权限已经和 RISC-V 对过，没有另写专有条目
