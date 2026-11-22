# 实验五（ch8）：并发与同步 —— 死锁检测

## 1. 实验目标

第八章在进程内引入多线程（`thread_create`/`gettid`/`waittid`）和三类同步原语（mutex、semaphore、condvar）。本章要在此基础上实现**死锁检测**：

| 接口 | 语义 |
| --- | --- |
| `sys_enable_deadlock_detect(bool)` | 为当前进程打开/关闭死锁检测 |
| 检测打开后的 `semaphore_down` | 若本次申请会让系统进入不安全状态，**不阻塞**，直接返回 `-0xdead` |
| 检测打开后的 `mutex_lock` | 同一线程重复加锁（自死锁）时返回 `-0xdead` |

同时本章的用例还依赖 `sys_get_time`（见 5.1，这是一个隐蔽的坑）。

## 2. 实验环境

`nightly-2024-05-02`、`riscv64gc-unknown-none-elf`、cargo-binutils 0.4.0、QEMU 8.2.2、RustSBI 0.4.1；checker `7d61ec5`、测试集 `a059366`。命令 `make -C ci-user test CHAPTER=8`。

## 3. 设计

### 3.1 需要记录的状态

死锁检测本质上是「资源分配图」上的安全性判断，先要把图建出来：

```rust
// ProcessControlBlockInner
pub deadlock_detect: bool,

// TaskControlBlockInner（每个线程）
pub sem_allocated: Vec<usize>,  // 当前持有各信号量多少个单位
pub sem_waiting: Option<usize>, // 正阻塞在哪个信号量上
pub mutex_held: Vec<usize>,     // 当前持有哪些 mutex id
```

`sem_allocated` 在 `semaphore_down` 成功返回后 +1、在 `semaphore_up` 时 -1；`sem_waiting` 在进入阻塞前设置、`down` 返回后清除。

### 3.2 信号量：银行家式安全性检查

`semaphore_down` 只有在资源不足（`count <= 0`）时才可能死锁，因此只在「本次将阻塞」时检查：

```
把当前线程看成「即将阻塞在 sem_id 上」，
然后反复寻找「可以跑完的线程」：
  没有阻塞请求的线程可以立刻跑完；
  阻塞请求能被当前 available 满足的线程也可以跑完；
  线程跑完 = 释放它持有的全部 sem_allocated。
若能跑完所有线程 → 安全，允许阻塞；
否则 → 不安全，返回 -0xdead，不阻塞。
```

核心代码：

```rust
let mut available: Vec<isize> = /* 每个信号量的 count（负数按 0 算） */;
let mut threads: Vec<(Option<usize>, Vec<usize>)> = /* (阻塞的 sem, 持有量) */;
let mut finished = vec![false; threads.len()];
let mut progress = true;
while progress {
    progress = false;
    for (i, (pending, allocated)) in threads.iter().enumerate() {
        if finished[i] { continue; }
        let can_finish = match pending {
            None => true,
            Some(j) => available[*j] >= 1,
        };
        if can_finish { available[*j] -= 1; available += allocated; finished[i] = true; progress = true; }
    }
}
finished.iter().all(|f| *f)
```

为什么不能只做「等待图环路检测」：`ch8_deadlock_sem2` 里 tid1 等 sem1（被 tid3 持有）、tid3 等 sem2（被 tid1 持有），**图上有环**，但同进程里 tid2、tid4 不阻塞、会跑完并释放资源，环会被打破，系统并不死锁。银行家式模拟正是靠「不阻塞的线程可以完成并归还资源」来区分这两种情况。

### 3.3 一个关键细节：只看 `Blocked` 的线程才算在等

`sem_waiting` 的清除时机是 `down()` 返回之后。一个线程被 `up` 唤醒后（状态已变 `Ready`）到它真正被调度前，`sem_waiting` 仍是旧值。若据此把它当成「仍在等」，会把它算成无法完成，从而误判不安全——`ch8_deadlock_sem2` 第一次失败就是这个原因（24/25）。

修复：

```rust
let mut pending = if task_inner.task_status == TaskStatus::Blocked {
    task_inner.sem_waiting
} else {
    None
};
```

### 3.4 mutex：自死锁

非递归 mutex 被同一线程二次加锁会永久阻塞。在 `sys_mutex_lock` 里查一下当前线程的 `mutex_held`：

```rust
if detect && task.inner_exclusive_access().mutex_held.contains(&mutex_id) {
    return -0xdead;
}
```

`sys_mutex_unlock` 时从 `mutex_held` 中移除。

## 4. 修改清单

| 文件 | 改动 |
| --- | --- |
| `os/src/task/process.rs` | `ProcessControlBlockInner` 增加 `deadlock_detect` |
| `os/src/task/task.rs` | `TaskControlBlockInner` 增加 `sem_allocated`/`sem_waiting`/`mutex_held` |
| `os/src/sync/semaphore.rs` | 新增 `available()` 读取当前可用量 |
| `os/src/syscall/sync.rs` | 实现 `sys_enable_deadlock_detect`；`mutex_lock/unlock` 记录持有；`semaphore_down` 做安全性检查、记录分配；`semaphore_up` 释放记录；新增 `sem_request_is_safe` |
| `os/src/syscall/process.rs` | 实现 `sys_get_time`（见 5.1） |

## 5. 测试与结果

### 5.1 第一个坑：用例卡死，但和死锁检测无关

首次运行 23/25，`ch8_deadlock_sem1`、`ch8_deadlock_sem2` 两条 FAIL，现象是**整个 QEMU 跑到 timeout 被杀**，这两个用例没有任何输出。

排查过程：给调度器、定时器、`sys_sleep` 都加了调试输出，发现：

- `check_timer` 每 10ms 正常触发（说明系统没死）；
- 但 `sys_sleep` 一次都没被调用；
- 用户进程一直在跑 `yield`(124) + `gettimeofday`(169)。

结论在用户库里：

```rust
pub fn sleep(period_ms: usize) {
    let start = get_time();
    while get_time() < start + period_ms as isize { sys_yield(); }
}
```

用户态 `sleep` 是**忙等**，依赖 `get_time()`。而 ch8 模板里 `sys_get_time` 仍是 `-1` 占位，于是 `get_time()` 恒为 -1，`while` 条件永真 → 测试的 main 永远走不到后面的 `semaphore_up(SEM_BARRIER)`，三个线程全部卡在屏障上。

实现 `sys_get_time`（把 `TimeVal` 按页表分段写入用户内存，避免跨页）后，用例才真正开始跑。这个问题的教训是：**死锁检测没生效的表象，可能根本不是检测算法的问题**，要看系统在等什么。

### 5.2 第二个坑：误判安全（24/25）

`sem1` 通过后 `sem2` 报 `assertion failed: failed == 0`——有线程被错误地「检测到死锁」。原因是 3.3 描述的 `sem_waiting` 过期问题：刚被唤醒、尚未被调度的线程仍带着旧的等待标记，被当成「无法完成」，于是安全性检查误判。

### 5.3 最终结果：25/25

```
Test passed26894: 25/25
Report for lab1 found.
Report for lab2 found.
Report for lab3 found.
Report for lab4 found.
```

覆盖情况：

| 用例 | 验证点 |
| --- | --- |
| `ch8_deadlock_mutex1` | 同线程二次加锁返回 `-0xdead` |
| `ch8_deadlock_sem1` | 三个线程构成环路，检测到死锁，至少一个线程返回值非 0 |
| `ch8_deadlock_sem2` | 图上存在环但实际可解开，**不能**误报死锁 |
| `ch8b_sync_sem` / `ch8b_mpsc_sem` | 常规信号量生产者消费者 |
| `ch8b_phil_din_mutex` | 哲学家就餐（mutex）不死锁 |
| `ch8b_test_condvar` | 条件变量 |
| `ch8b_threads` / `ch8b_threads_arg` | 线程创建、参数、`waittid` |
| `ch7b_pipetest` / `ch6b_*` / `ch5b_*` / `ch3b_*` | 前序章节功能回归 |

## 6. 问题与思考

### 6.1 为什么检测结果要返回 `-0xdead` 而不是直接杀进程

系统调用有明确的返回值约定：`-1` 表示参数错误，`-0xdead` 是本章约定的「检测到死锁」。把决定权交给用户态，用户程序可以自行降级（打印、退出、释放资源）。若内核直接杀进程，用户态就无法区分「死锁」和「程序 bug」，也无法做兜底处理。

### 6.2 为什么只在 `down` 时会检测

`up` 只会增加可用资源、唤醒等待者，不可能让系统变得更不安全；`mutex` 的解锁同理。检测放在「可能让线程永久阻塞」的位置即可，减少无谓开销。

### 6.3 银行家算法依赖的假设

本实现假定「不阻塞的线程最终会完成并释放资源」。这在测试场景下成立，但严格来说银行家算法需要每个线程预先声明最大需求（max claim）。本实验没有这个信息，只能用「当前请求 + 当前持有」做保守估计，因此是**近似**的：极端交错下仍可能漏报或误报。

## 7. 不足与后续

- 检测只覆盖同一进程内的线程（信号量、mutex 都是进程内的），跨进程资源（如文件锁）没有建模。
- `sem_allocated` 在线程异常退出时不会自动归还；进程内若有线程在持锁状态下崩溃，后续安全性判断会偏保守。
- 每次 `semaphore_down` 都要遍历所有线程做模拟，复杂度 O(线程数² × 迭代轮数)，线程数很大时开销可观。
