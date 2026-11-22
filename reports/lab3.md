# 实验三（ch5）：进程管理 —— spawn 与 stride 调度

## 1. 实验目标

第五章把「任务」升级为「进程」，每个进程有独立的进程号、父子关系和地址空间。本章在已给出的 `fork`/`exec`/`waitpid` 基础上补两块：

| 功能 | 要求 |
| --- | --- |
| `sys_spawn` | 以 ELF 名字创建全新进程，不复制当前地址空间 |
| `sys_set_priority` | 设置当前进程的 stride 调度优先级，返回新优先级；非法值返回 `-1` |
| 调度器 | 由 FIFO 改为 stride 调度（按 `pass` 最小者优先） |

同时把第四章的 `sys_get_time` / `sys_mmap` / `sys_munmap` 迁移到进程模型上（ch5 的用例仍包含 ch4 的 mmap/unmap）。

## 2. 实验环境

`nightly-2024-05-02`、`riscv64gc-unknown-none-elf`、cargo-binutils 0.4.0、QEMU 8.2.2、RustSBI 0.4.1；checker `7d61ec5`、测试集 `a059366`。命令 `make -C ci-user test CHAPTER=5`。

## 3. 设计

### 3.1 `spawn`：为什么不是 `fork` + `exec`

`fork` 会完整复制父进程地址空间（`MemorySet::from_existed_user` 逐页拷贝），然后 `exec` 再把整个地址空间丢弃换成新的——两次开销都是白费。`spawn` 直接用 `ELF` 构造一个新的 TCB：

```rust
pub fn sys_spawn(path: *const u8) -> isize {
    let parent = current_task().unwrap();
    let path = translated_str(current_user_token(), path);
    let elf_data = match get_app_data_by_name(path.as_str()) {
        Some(data) => data,
        None => return -1,
    };
    let child = Arc::new(TaskControlBlock::new(elf_data));
    {   // 先设置父子关系，再挂到父进程的 children 里
        child.inner_exclusive_access().parent = Some(Arc::downgrade(&parent));
    }
    let pid = child.getpid();
    parent.inner_exclusive_access().children.push(child.clone());
    add_task(child);
    pid as isize
}
```

注意 `parent` 字段用 `Weak`：子进程生命周期可能长于父进程，强引用会阻止父进程 TCB 释放；而 `children` 用 `Arc`，因为父进程 `waitpid` 时必须能访问子进程的退出码。

### 3.2 stride 调度

给每个进程在 `TaskControlBlockInner` 里加三个字段：

```rust
pub priority: usize,   // 优先级，默认 16
pub stride: usize,     // BIG_STRIDE / priority
pub pass: usize,       // 已占用时间片的加权累计
```

- `BIG_STRIDE = 1 << 20`。`stride` 与优先级成反比：优先级越高，每轮消耗的 `pass` 越多，被调度的频率就越高，长期比例接近 `priority` 之比。
- 调度器每次从就绪队列里挑 `pass` 最小的进程，取出后立即 `pass += stride`：

```rust
pub fn fetch(&mut self) -> Option<Arc<TaskControlBlock>> {
    let mut selected = None; let mut min_pass = usize::MAX;
    for (index, task) in self.ready_queue.iter().enumerate() {
        let pass = task.inner_exclusive_access().pass;
        if pass < min_pass { min_pass = pass; selected = Some(index); }
    }
    let index = selected?;
    let task = self.ready_queue.remove(index)?;
    { let mut inner = task.inner_exclusive_access(); inner.pass += inner.stride; }
    Some(task)
}
```

**一个容易忽略的坑**：新进程如果 `pass = 0`，它会在所有老进程之前连续被调度，造成"插队"。因此在 `add` 时把新进程的 `pass` 抬到当前就绪队列的最小 `pass`：

```rust
let min_pass = self.ready_queue.iter().map(|t| t.inner_exclusive_access().pass).min();
if let Some(min_pass) = min_pass { if task.pass < min_pass { task.pass = min_pass; } }
```

`fork` 出来的子进程继承父进程的 `priority` / `stride` / `pass`，语义上等价于"子进程从父进程当前的时间片状态继续"。

### 3.3 `set_priority` 的取值规则

测试要求：

```rust
assert_eq!(set_priority(10), 10);
assert_eq!(set_priority(isize::MAX), isize::MAX);
assert_eq!(set_priority(0), -1);
assert_eq!(set_priority(1), -1);
assert_eq!(set_priority(-10), -1);
```

因此合法区间是 `prio >= 2`，上界不限（`isize::MAX` 也要接受）。实现里 `stride = BIG_STRIDE / prio as usize`，当 `prio` 很大时 `stride` 变成 0，进程几乎不会被 `pass` 推后，但不会 panic。若直接把用户传的 `isize` 当 `usize` 用，负数会变成巨大的正数被误接受，所以必须先判 `prio < 2`。

### 3.4 ch4 功能的迁移

进程模型下 `memory_set` 在 `TaskControlBlockInner` 里，因此 `mmap`/`munmap` 变成 TCB 上的方法（内部 borrow 一下 `inner`），`sys_mmap` / `sys_munmap` 只做参数校验（长度、对齐、`prot` 位），再交给 TCB。`sys_get_time` 仍然走 `translated_byte_buffer_try` 逐段拷贝，避免 `TimeVal` 跨页。

## 4. 修改清单

| 文件 | 改动 |
| --- | --- |
| `os/src/config.rs` | 新增 `BIG_STRIDE`、`DEFAULT_PRIORITY` |
| `os/src/task/task.rs` | TCB inner 增加 `priority`/`stride`/`pass`（`new`、`fork` 中初始化）；新增 `set_priority`、`mmap`、`munmap` |
| `os/src/task/manager.rs` | FIFO → stride：`fetch` 取最小 `pass` 并累加 `stride`；`add` 时校正新进程 `pass` |
| `os/src/syscall/process.rs` | 实现 `sys_spawn`、`sys_set_priority`、`sys_get_time`、`sys_mmap`、`sys_munmap` |
| `os/src/mm/page_table.rs` | 新增 `translated_byte_buffer_try`（失败返回 `None`） |
| `os/src/mm/memory_set.rs` | 新增区间冲突检测与 `mmap`/`munmap` |
| `os/src/mm/mod.rs` | 导出新增函数 |

## 5. 测试与结果

一次通过：

```
Usertests: Test ch5_spawn0   in Process 16 exited with code 0
Usertests: Test ch5_spawn1   in Process 17 exited with code 0
Usertests: Test ch5_setprio  in Process 18 exited with code 0
Usertests: Running ch5_stride
ch5 Usertests passed34766!

Test passed34766: 15/15
Report for lab1 found.
Report for lab2 found.
Report for lab3 needed.
```

覆盖情况：

| 用例 | 验证点 |
| --- | --- |
| `ch5_spawn0` | 连续 spawn 40 个 `getpid` 子进程并逐个 `wait`，退出码全 0；多余的 `wait` 返回 `<= 0` |
| `ch5_spawn1` | `wait`/`waitpid` 能取到正确的 pid 与退出码（含负退出码 `-233`） |
| `ch5_setprio` | 优先级边界值校验（10 与 `isize::MAX` 接受，0/1/负数拒绝） |
| `ch5_stride*` | 6 个不同优先级的进程按比例获得 CPU，全部正常退出 |
| `ch4_*` | mmap/unmap 用例在新进程模型下仍全部通过 |
| `ch5b_*` | fork / forktree / 用户 shell 等基础用例通过 |

`ch5_usertest` 最后输出 `ch5 Usertests passed!`，所有子测试退出码为 0（mmap1/mmap2 是被内核杀掉的预期异常，退出码 `-2`）。

## 6. 问题与思考

### 6.1 为什么 `spawn` 里要先设置 `parent` 再入队

如果把子进程先 `add_task` 交给调度器，它可能在父进程执行 `children.push()` 之前就跑完并退出。退出时代码会沿 `parent` 把它的子进程挂到 `initproc` 下，而父进程的 `children` 此时还没有它，`waitpid` 就会永远等不到这个僵尸进程。所以顺序必须是：构造 TCB → 设置父子引用 → 父进程登记 → 入就绪队列。

### 6.2 stride 与"公平"的度量

`pass` 是「已经消耗的加权时间」，不是「已经运行的时间」。高优先级进程 `stride` 小，`pass` 增长慢，于是被调度得更频繁；长期看每个进程获得的 CPU 比例 ≈ `priority / Σpriority`。`ch5_stride` 打印的 `ratio = count/priority` 在六个进程上大致相等，正是这一点的体现。

### 6.3 优先级为 `isize::MAX` 时 stride 会变成 0

`BIG_STRIDE / isize::MAX == 0`，该进程的 `pass` 不再增长，相当于"永远优先"。测试允许这一取值，说明实验只要求接口健壮（不 panic、返回正确值），并不要求对极端值做业务限制。工程上更稳妥的做法是给优先级加上界（例如 100），但这会与测试用例冲突。

## 7. 不足与后续

- stride 调度器每次 `fetch` 都要遍历整个就绪队列（O(n)），进程数很多时应改用堆或跳表。
- `spawn` 未限制子进程数量，理论上可以耗尽物理页；`TaskControlBlock::new` 中的 `frame_alloc().unwrap()` 会 panic。
- 优先级没有上界保护，`stride = 0` 的进程会垄断 CPU。
