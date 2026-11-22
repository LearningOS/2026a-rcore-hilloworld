# 实验二（ch4）：地址空间

## 1. 实验目标

第四章引入基于 SV39 的页式虚拟内存，本章要把第三章的 `sys_trace` 迁移到「按页表访问用户内存」，并新实现三个系统调用：

| 系统调用 | 语义 |
| --- | --- |
| `sys_get_time` | 写 `TimeVal { sec, usec }` 到用户地址，**可能跨页** |
| `sys_trace` | 0 读用户字节 / 1 写用户字节 / 2 查询系统调用计数 |
| `sys_mmap` | 在用户地址空间映射一段可读/可写/可执行的匿名内存 |
| `sys_munmap` | 解除一段映射并回收物理页 |

与 ch3 最大的不同：内核不再能拿用户指针直接解引用，所有用户内存访问都必须走页表。

## 2. 实验环境

`nightly-2024-05-02`、`riscv64gc-unknown-none-elf`、cargo-binutils 0.4.0、QEMU 8.2.2、RustSBI 0.4.1；checker `7d61ec5`、测试集 `a059366`。运行命令 `make -C ci-user test CHAPTER=4`。

## 3. 设计

### 3.1 系统调用计数：沿用 ch3，随任务存放在 TCB

ch4 的 `TaskControlBlock` 已经不是 `Copy`（内部持有 `MemorySet`），直接加字段即可：

```rust
pub syscall_times: [u32; MAX_SYSCALL_NUM],
```

计数仍然放在 `syscall()` 入口、`match` 之前，保证「查询自身也被计入」，这样 `ch3_trace` 用例在 ch4 上依然能通过。

### 3.2 `sys_get_time`：跨页写入

`TimeVal` 是两个 `usize`，共 16 字节。若用户传入的指针落在 `page_size - 8` 之后，这 16 字节会**横跨两个物理页**，因此不能写成 `*ts = TimeVal { .. }`。做法是先把 `TimeVal` 序列化成字节切片，再用页表把目标区间翻译成若干片段依次拷贝：

```rust
let src = unsafe { from_raw_parts(&time_val as *const _ as *const u8, size_of::<TimeVal>()) };
match translated_byte_buffer_try(token, ts as *const u8, src.len()) {
    Some(buffers) => { /* 逐段 copy_from_slice */ 0 }
    None => -1,
}
```

原来的 `translated_byte_buffer` 内部对未映射页 `unwrap()`，用户传坏指针会直接 panic 内核。为此新增 `translated_byte_buffer_try`，返回 `Option`，原函数改为调用它并 `expect`，`sys_write` 等既有调用点行为不变。

### 3.3 `sys_trace`：单字节访问 + 权限 + 地址范围

单字节访问用「页表项 → 物理页 → 页内偏移」直接定位：

```rust
fn user_byte(token: usize, addr: usize) -> Option<(*mut u8, bool, bool)> {
    if addr >= MAX_USER_VA { return None; }          // 关键校验，见 5.1
    let pte = PageTable::from_token(token).translate(va.floor())?;
    if !pte.is_valid() { return None; }
    let ptr = pte.ppn().get_bytes_array().as_mut_ptr().wrapping_add(va.page_offset());
    Some((ptr, pte.readable(), pte.writable()))
}
```

- 读要求 PTE 的 `R` 位，写要求 `W` 位。注意内核通过物理地址写用户页**不受 PTE 的 W 位约束**，必须自己检查，否则 `mmap(prot=READONLY)` 之后仍能写穿，`ch4_trace1` 的 `assert_eq!(-1, trace_write(...))` 就会失败。
- 访问使用 `read_volatile` / `write_volatile`。

### 3.4 `mmap` / `munmap`：在 `MemorySet` 上实现

在 `MemorySet` 增加两个方法，由 `TaskManager` 转发到当前任务的 `memory_set`：

```rust
pub fn mmap(&mut self, start_va, end_va, permission) -> bool {
    if start_va.page_offset() != 0 { return false; }        // 起始必须页对齐
    let (start_vpn, end_vpn) = (start_va.floor(), end_va.ceil());
    if start_vpn >= end_vpn || self.overlaps(start_vpn, end_vpn) { return false; }
    self.insert_framed_area(start_va, end_va, permission);  // Framed：分配新物理页并清零
    true
}
```

要点：

- **只要求起始地址对齐，长度按 `ceil` 向上取整**。`ch4_unmap` 里 `mmap(start - len, len + 1, prot)` 必须成功，说明 `len` 不要求是页大小整数倍；而 `mmap(start + len + 1, len, prot)` 必须失败，说明起点必须对齐。
- **冲突检测遍历所有已映射区域**（代码段、堆、栈、trap context、trampoline 都在 `areas` 里），区间相交即拒绝。`mmap(start - len, len + 1)` 之所以失败，正是因为它向上取整后与已映射区域相交。
- `munmap` 要求 `start` 页对齐、`len` 为页整数倍，且**必须与某个已映射区域完全重合**才会成功；成功后 `unmap` 每页并从 `areas` 中移除，`FrameTracker` 随之释放物理页。

权限转换是本章另一个易错点：用户态 `prot` 的位含义是 `bit0=R, bit1=W, bit2=X`（`mmap(3)` 表示读写），而内核 `MapPermission` 的位是 `R=1<<1, W=1<<2, X=1<<3, U=1<<4`，需要逐位转换并补上 `U`：

```rust
if port == 0 || port & !0b111 != 0 { return -1; }
let mut permission = MapPermission::U;
if port & 0b001 != 0 { permission |= MapPermission::R; }
if port & 0b010 != 0 { permission |= MapPermission::W; }
if port & 0b100 != 0 { permission |= MapPermission::X; }
```

## 4. 修改清单

| 文件 | 改动 |
| --- | --- |
| `os/src/task/task.rs` | TCB 增加 `syscall_times` 并在 `new` 中清零 |
| `os/src/task/mod.rs` | 计数/查询、`mmap`/`munmap` 的转发方法及模块级函数 |
| `os/src/syscall/mod.rs` | 分发前计数 |
| `os/src/syscall/process.rs` | 实现 `sys_get_time`、`sys_trace`、`sys_mmap`、`sys_munmap` |
| `os/src/mm/page_table.rs` | 新增 `translated_byte_buffer_try` |
| `os/src/mm/memory_set.rs` | 新增区间冲突检测与 `mmap` / `munmap` |
| `os/src/mm/mod.rs` | 导出新增函数 |

## 5. 测试与结果

### 5.1 第一次运行：15/16

唯一失败在 `ch4_trace1.rs:17`：

```
Panicked at src/bin/ch4_trace1.rs:17, assertion `left == right` failed
  left: None
 right: Some(0)
```

即 `trace_read(isize::MAX)` 期望 `None`，实际返回了 `Some(0)`。

原因值得记录：SV39 的页表索引只取页号的**低 27 位**（9+9+9）。`isize::MAX = 0x7FFF_FFFF_FFFF_FFFF` 的页号是 `0x7FF_FFFF_FFFF_F`，其低 27 位全为 1；而内核映射在顶部的 trampoline 页号 `0xFFFF_FFFF_FFFF_F` 低 27 位也全为 1。二者索引完全相同，于是 `isize::MAX` 实际命中了 trampoline 的页表项，读到的是那块物理页偏移 `0xFFF` 处的字节（值为 0）。

修复：在 `user_byte` 里先判断 `addr >= 1 << 38`（SV39 用户空间上界）直接返回 `None`。这既是正确性要求（trace 只能访问用户内存），也顺带避免了非规范地址（non-canonical）的别名问题。

### 5.2 第二次运行：16/16

```
Test passed19725: 16/16
Report for lab1 found.
Report for lab2 needed.
```

覆盖到的用例：

| 用例 | 验证点 |
| --- | --- |
| `ch4_mmap0` | 映射后可读可写，写入与读回一致 |
| `ch4_mmap1` | `prot=1`（只读）下写入触发访存异常，进程被杀 |
| `ch4_mmap2` | `prot=2`（只写）下读取触发异常 |
| `ch4_mmap3` | 非对齐起点、区间冲突、`prot=0`、`prot|8` 均返回 -1 |
| `ch4_unmap` / `ch4_unmap2` | 正常解除；长度非整页、起点非对齐返回 -1 |
| `ch4_trace1` | 读/写权限、非法地址、映射前后的可访问性变化 |
| `ch4b_sbrk` | 堆扩展与收缩，收缩后写入触发缺页 |
| `ch3_*` | 旧用例在地址空间下仍然通过 |

## 6. 问题与思考

### 6.1 为什么内核写用户页要自己检查 W 位

页表项里的 `W` 只约束 **U 模式** 的访存。内核（S 模式）通过物理地址访问帧时，硬件不做权限检查。所以 `trace_write` 必须显式检查 `pte.writable()`，否则只读映射形同虚设。同理 `trace_read` 要检查 `R` 位。

### 6.2 为什么 `mmap` 不要求 `len` 对齐

用户给出的 `len` 是字节数，内核按页向上取整即可；强制要求页对齐反而会让 `mmap(start - len, len + 1)` 这类调用失败，与测试语义不符。真正需要严格对齐的是 `munmap` 的 `start` 和 `len`，因为解除映射必须精确到页，否则会出现"解除了半个页"的语义问题。

### 6.3 权限位不匹配的典型现象

如果直接把用户 `prot` 当作 `MapPermission` 用，`mmap(3)` 会得到 `R|U`（因为 `MapPermission::R=1<<1`，而 3 的 bit0 被忽略），于是 `ch4_mmap0` 的写入会缺页；反过来如果漏掉 `U` 位，用户态访问会全部 fault。位定义必须显式转换。

## 7. 不足与后续

- `mmap` 只检查了起点对齐和区间冲突，没有限制映射范围必须位于用户空间，恶意的超大 `len` 会耗尽物理页（`frame_alloc().unwrap()` 会 panic）。
- 物理页是**立即分配**的，没有做懒分配；大量 `mmap` 会立刻吃掉内存。
- `trace` 的地址校验放在 `user_byte` 里，`sys_get_time` 没有做同样的上界检查（依赖调用方传入合法指针）。
