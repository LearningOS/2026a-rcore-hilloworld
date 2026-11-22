# 实验一（ch3）：系统调用计数 `sys_trace`

## 1. 实验目标

在第三章多道程序与分时多任务的内核上，新增一个 `sys_trace`（系统调用号 410）系统调用，实现三种能力：

| `trace_request` | 语义 | 返回值 |
| --- | --- | --- |
| 0 (`Read`) | 读取用户地址 `id` 处的一个字节 | 该字节；失败返回 `-1` |
| 1 (`Write`) | 把 `data as u8` 写入用户地址 `id` | `0`；失败返回 `-1` |
| 2 (`Syscall`) | 查询当前任务对系统调用 `id` 的调用次数 | 次数；越界返回 `-1` |

其中计数要求**按任务隔离**，并且 `count_syscall(SYSCALL_TRACE)` 的返回值要**包含这次查询自身**。

## 2. 实验环境

| 组件 | 版本 |
| --- | --- |
| Rust 工具链 | `nightly-2024-05-02`（rustc 1.80.0-nightly, `c987ad527 2024-05-01`） |
| 目标平台 | `riscv64gc-unknown-none-elf` |
| cargo-binutils | `0.4.0`（`rust-objcopy` / `rust-objdump`） |
| QEMU | `8.2.2`（自源码构建） |
| RustSBI 固件 | `0.4.1`（仓库自带 `bootloader/rustsbi-qemu.bin`，jump 模式） |
| 官方 checker | `rCore-Tutorial-Checker` @ `7d61ec5` |
| 用户测试集 | `rCore-Tutorial-Test` @ `a059366` |

运行方式：`make -C ci-user test CHAPTER=3`。checker 会把 `os/`、`user/`、`check/` 备份后随机化输出标记（本次运行为 `13780` / `46426`），跑完再还原。

## 3. 设计

### 3.1 计数存放位置：放进 TCB

计数必须属于「某个任务」，因此统一放在任务控制块中：

```rust
pub struct TaskControlBlock {
    pub task_status: TaskStatus,
    pub task_cx: TaskContext,
    /// 每个系统调用被本任务调用的次数
    pub syscall_times: [u32; MAX_SYSCALL_NUM],
}
```

`MAX_SYSCALL_NUM` 取 500，覆盖 RISC-V Linux 兼容编号（最大约 450）并在尾部留出余量。`u32` 足够：单任务 40 亿次调用远超实验规模。

内存开销：`MAX_APP_NUM = 16` 个 TCB，每块 `500 × 4 B ≈ 2 KB`，合计约 32 KB 静态数据，远小于 `MEMORY_END = 0x88000000` 的可用范围，不影响内核镜像装载。

### 3.2 计数时机：分发处，而不是 `sys_trace` 内

关键设计点。计数放在 `syscall()` 统一入口、`match` 分发**之前**：

```rust
pub fn syscall(syscall_id: usize, args: [usize; 3]) -> isize {
    crate::task::add_syscall_count(syscall_id);
    match syscall_id {
        SYSCALL_WRITE => sys_write(...),
        ...
    }
}
```

原因有两个：

1. **所有系统调用都走这一个入口**，不需要在每个 `sys_*` 函数里各写一遍，也不会漏掉 `sys_exit`、`sys_yield` 这类会切换任务或永不返回的调用。
2. **满足「计数含自身」的语义**：先 `+1` 再返回计数，`count_syscall(TRACE)` 第一次得到 1、第二次得到 2，与测试 `assert_eq!(2, count_syscall(SYSCALL_TRACE))` 一致。若把 `+1` 写进 `sys_trace` 内部，查询结果永远比预期少 1。

### 3.3 `sys_trace` 的三个分支

```rust
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    match trace_request {
        0 => unsafe { core::ptr::read_volatile(id as *const u8) as isize },
        1 => { unsafe { core::ptr::write_volatile(id as *mut u8, data as u8) }; 0 }
        2 => get_syscall_count(id),
        _ => -1,
    }
}
```

两点说明：

- **必须使用 `read_volatile` / `write_volatile`**。测试会 `trace_read(main as *const _)` 读取代码段首字节，普通解引用可能被优化器当作无用读写消除，导致读到旧值或根本不产生访存。
- 第三章内核尚未建立独立页表，用户地址空间与内核在同一地址视图下，用户指针可以直接解引用；到第四章引入地址空间后，这里必须改为经页表翻译的 `translated_byte_buffer` 等方法，不能直接照搬。

### 3.4 任务管理器接口

在 `TaskManager` 上增加按「当前任务」操作的计数接口，并导出模块级函数，避免在 `syscall` 模块里直接访问全局 `TASK_MANAGER`：

```rust
fn add_syscall_count(&self, id: usize) {
    if id < MAX_SYSCALL_NUM {
        let mut inner = self.inner.exclusive_access();
        let current = inner.current_task;
        inner.tasks[current].syscall_times[id] += 1;
    }
}

fn get_syscall_count(&self, id: usize) -> isize {
    if id < MAX_SYSCALL_NUM {
        let inner = self.inner.exclusive_access();
        inner.tasks[inner.current_task].syscall_times[id] as isize
    } else {
        -1
    }
}
```

`current_task` 由调度器在 `run_next_task` 中维护，因此计数天然跟随任务切换，无需在上下文切换时额外搬运。

## 4. 修改清单

| 文件 | 改动 |
| --- | --- |
| `os/src/config.rs` | 新增 `MAX_SYSCALL_NUM = 500` |
| `os/src/task/task.rs` | TCB 增加 `syscall_times: [u32; MAX_SYSCALL_NUM]` |
| `os/src/task/mod.rs` | TCB 初始化清零；`TaskManager` 增加计数/查询方法并导出 |
| `os/src/syscall/mod.rs` | `syscall()` 分发前计数 |
| `os/src/syscall/process.rs` | 实现 `sys_trace` 三分支 |

## 5. 测试与结果

### 5.1 基线（实现前）

模板中 `sys_trace` 返回 `-1` 占位，checker 结果：

```
[FAIL] not found <string from task trace test>
[FAIL] not found <Test trace OK13780!>
Test passed13780: 5/7
```

程序在 `ch3_trace.rs:22` 处 `assert!(3 <= count_syscall(SYSCALL_GETTIMEOFDAY))` 失败：
计数恒为 `-1`，断言不成立。

### 5.2 实现后

```
[PASS] found <get_time OK46426! (\d+)>
[PASS] found <Test sleep OK46426!>
[PASS] found <current time_msec = (\d+)>
[PASS] found <time_msec = ... delta = (\d+)ms!>
[PASS] found <Test sleep1 passed46426!>
[PASS] found <string from task trace test>
[PASS] found <Test trace OK46426!>

Test passed46426: 7/7
```

7 项全部通过。`ch3_trace.rs` 中的各条断言（`GETTIMEOFDAY ≥ 3`、`TRACE == 2`、`WRITE == 0/2`、`YIELD > 0`、`EXIT == 0`）均成立，说明按任务计数的语义和「含自身」的时机都正确。

## 6. 问题与思考

### 6.1 为什么 `println!` 会产生 2 次 `write`

测试在 `println!("string from task trace test\n")` 之后断言 `count_syscall(SYSCALL_WRITE) == 2`。原因是：

- `println!` 宏的实现是 `format_args!(concat!($fmt, "\n"))`，即**自动追加一个换行**；
- 这里的字面量本身已经以 `\n` 结尾，拼接后得到两个换行；
- `ConsoleBuffer::write_str` 每遇到 `\n` 就调用一次 `flush()`，每次 `flush()` 对应一次 `write` 系统调用。

因此两个换行 = 两次 `write`。这也说明计数器统计的是真实的系统调用次数，而不是「逻辑上的打印次数」。

### 6.2 计数时机差一的典型错误

如果把 `syscall_times[syscall_id] += 1` 写在 `sys_trace` 函数体内，那么 `count_syscall(SYSCALL_TRACE)` 返回的是「本次调用之前」的次数，第一条关于 `TRACE` 的断言会得到 1 而不是 2。把计数放在分发入口、`match` 之前，语义才是「这次调用已经发生」。

### 6.3 为什么读用户字节要用 `volatile`

`trace_read(&var)` 中 `var` 是用户程序的局部变量，内核通过裸指针访问。编译器看不到该内存被「外部」修改，普通读写可能被优化掉；使用 `read_volatile` / `write_volatile` 强制每次访问都真实发生，这也是 `trace_write` 后能立即 `read_volatile` 读到新值的前提。

## 7. 不足与后续

- ch3 阶段没有做地址合法性检查，非法用户指针会导致内核缺页；第四章引入地址空间后应通过页表翻译接口访问，并对地址范围做校验。
- 计数为 `u32`，未处理溢出；实验规模下不会触发，但工程上应改为饱和计数或 `u64`。
- 任务退出后计数随 TCB 一起失效，若后续需要跨生命周期统计，应改到进程级别（ch5 起 TCB 会拆分为进程与任务）。
