# 实验四（ch6）：文件系统与 I/O —— 硬链接与 fstat

## 1. 实验目标

第六章把用户程序、数据都放到 easy-fs（块设备上的只读/可写文件系统）里，并实现完整的文件系统调用。本章需要补三块：

| 系统调用 | 语义 |
| --- | --- |
| `sys_fstat` | 把打开文件的 `Stat`（设备号、inode 号、类型、硬链接数）写到用户内存 |
| `sys_linkat` | 为已有文件创建一个硬链接（新的目录项指向同一个 inode） |
| `sys_unlinkat` | 删除一个目录项并减少目标 inode 的硬链接数 |

同时把第四章（`mmap`/`munmap`/`get_time`）和第五章（`spawn`）的功能迁移到 ch6 的进程模型与文件系统上——ch6 的用例清单里仍然包含这些用例。

## 2. 实验环境

`nightly-2024-05-02`、`riscv64gc-unknown-none-elf`、cargo-binutils 0.4.0、QEMU 8.2.2、RustSBI 0.4.1；checker `7d61ec5`、测试集 `a059366`。命令 `make -C ci-user test CHAPTER=6`。

## 3. 设计

### 3.1 `nlink`：磁盘 inode 上的新字段

硬链接数是文件的属性，必须落盘，因此加在 `DiskInode` 中（`#[repr(C)]` 保证布局稳定）：

```rust
pub struct DiskInode {
    pub size: u32,
    pub nlink: u32,        // 新增
    pub direct: [u32; INODE_DIRECT_COUNT],
    pub indirect1: u32,
    pub indirect2: u32,
    type_: DiskInodeType,
}
```

`initialize` 时置 1（新建文件至少有一个目录项）。`DiskInode` 尺寸从 128 B 变成 132 B，`get_disk_inode_pos` 里的 `inodes_per_block` 是运行时用 `size_of::<DiskInode>()` 算的，会自动适应；磁盘镜像由 `easy-fs-fuse` 用同一个 crate 生成，因此布局天然一致。

### 3.2 `link` / `unlink`：目录项的增删

`link` 在**目录 inode** 上实现：先确认新名字不存在，再把目标 inode 的 `nlink += 1`，最后在目录末尾追加一个 `DirEntry`。

```rust
pub fn link(&self, name: &str, inode: &Arc<Inode>) -> Option<usize> {
    let mut fs = self.fs.lock();
    if self.read_disk_inode(|d| self.find_inode_id(name, d)).is_some() { return None; }
    let inode_id = fs.get_inode_id(inode.block_id as u32, inode.block_offset);
    inode.modify_disk_inode(|d| d.nlink += 1);   // 单独一次块缓存锁
    self.modify_disk_inode(|dir| { /* increase_size + write_at 追加 dirent */ });
    block_cache_sync_all();
    Some(inode_id as usize)
}
```

这里刻意**不嵌套块缓存锁**：目标 inode 与目录 inode 很可能在同一个磁盘块里（都在 inode 区），如果在持有目录块缓存锁的时候再去锁目标 inode 的块，会直接死锁。所以先改 `nlink`、释放锁，再改目录。

`unlink` 反过来：先减 `nlink`，再把目录项删掉。删除用「把最后一项搬到空位、目录大小减一项」的做法：

```rust
let (block_id, block_offset) = fs.get_disk_inode_pos(inode_id);
let target = Inode::new(block_id, block_offset, self.fs.clone(), self.block_device.clone());
let nlink = target.modify_disk_inode(|d| { if d.nlink > 0 { d.nlink -= 1; } d.nlink });
self.modify_disk_inode(|dir| {
    // 找到 name 所在槽位 slot，把最后一项写到 slot，然后 size -= DIRENT_SZ
});
```

目录大小减小后，原来占用最后一个数据块可能变成空块；按实验说明「不回收 inode 与数据块也不算错」，这里不做 `dealloc_data`，只把目录项去掉。

### 3.3 `fstat`：把 `Stat` 抄到用户空间

`Stat` 是 `#[repr(C)]` 的 80 字节结构（`dev`/`ino`/`mode`/`nlink` + 7 个 `u64` 填充）。为了让 `fd_table` 里的 `Arc<dyn File>` 能取到它，给 `File` trait 加了一个带默认实现的方法：

```rust
fn stat(&self) -> Stat { Stat::empty() }     // Stdin/Stdout 用默认值
```

`OSInode` 覆写为：

```rust
fn stat(&self) -> Stat {
    let inner = self.inner.exclusive_access();
    let inode = &inner.inode;
    Stat {
        dev: 0,
        ino: inode.inode_id() as u64,
        mode: if inode.is_file() { StatMode::FILE } else { StatMode::DIR },
        nlink: inode.nlink(),
        pad: [0; 7],
    }
}
```

`sys_fstat` 用 `translated_byte_buffer_try` 逐段写入用户内存（`Stat` 也可能跨页），失败返回 `-1`。

`dev` 固定 0：测试只要求「同一文件两次 `fstat` 的 `dev`/`ino` 一致」，不要求与真实块设备号绑定；`ino` 用 `inode_id`，通过 `fs.get_inode_id(block_id, block_offset)` 从磁盘位置反推。

### 3.4 迁移 ch4/ch5 的功能

- `mmap`/`munmap`：在 ch6 的 `MemorySet` 上加区间冲突检测与 `mmap`/`munmap` 方法，TCB 提供转发方法，`syscall` 层只做参数校验。
- `sys_get_time`：用 `translated_byte_buffer_try` 逐段拷贝 `TimeVal`。
- `sys_spawn`：**ch6 与 ch5 有一个关键区别**——ch6 的用户程序不再编进内核镜像（`main.rs` 里没有 `mod loader`，也没有 `_num_app` 符号），而是作为文件存在 easy-fs 里。因此 `spawn` 不能再用 `get_app_data_by_name`，必须走文件系统：

```rust
let app_inode = open_file(path.as_str(), OpenFlags::RDONLY)?;
let elf_data = app_inode.read_all();
let child = Arc::new(TaskControlBlock::new(elf_data.as_slice()));
```

## 4. 修改清单

| 文件 | 改动 |
| --- | --- |
| `easy-fs/src/layout.rs` | `DiskInode` 增加 `nlink`，`initialize` 置 1 |
| `easy-fs/src/efs.rs` | 新增 `get_inode_id(block_id, block_offset)` |
| `easy-fs/src/vfs.rs` | 新增 `inode_id`、`nlink`、`is_file`、`link`、`unlink` |
| `os/src/fs/mod.rs` | `File` trait 增加 `stat()` 默认实现；`Stat::empty()`；导出 `ROOT_INODE` |
| `os/src/fs/inode.rs` | `OSInode::stat()`；`ROOT_INODE` 补充文档 |
| `os/src/syscall/fs.rs` | 实现 `sys_fstat`、`sys_linkat`、`sys_unlinkat` |
| `os/src/syscall/process.rs` | 实现 `sys_get_time`、`sys_mmap`、`sys_munmap`、`sys_spawn`（从文件系统读 ELF） |
| `os/src/mm/memory_set.rs` | 新增冲突检测与 `mmap`/`munmap` |
| `os/src/mm/page_table.rs` | 新增 `translated_byte_buffer_try` |
| `os/src/mm/mod.rs` | 导出新增函数 |
| `os/src/task/task.rs` | TCB 增加 `mmap`/`munmap` 转发方法 |

## 5. 测试与结果

一次通过：

```
Test passed1524: 31/31
Report for lab1 found.
Report for lab2 found.
Report for lab3 found.
Report for lab4 needed.
```

本章新增用例：

| 用例 | 验证点 |
| --- | --- |
| `ch6_file0` | 创建→写入→关闭→只读打开→读回内容一致 |
| `ch6_file1` | `fstat` 返回 `mode == FILE`、`nlink == 1` |
| `ch6_file2` | 连续 `link` 三次后 `nlink` 从 2 变 4；`unlink` 原文件名后通过链接名仍能读到内容，`dev`/`ino` 不变、`nlink` 递减 |
| `ch6_file3` | 10 轮「创建→写 50 次→关闭→unlink」，每轮 unlink 后 `open` 必须失败 |
| `ch6b_*` | cat、filetest_simple、initproc、user_shell 等文件系统基础用例 |

原有 `ch4_*`、`ch5_*`、`ch3b_*` 用例也全部通过（共 31 项）。

## 6. 问题与思考

### 6.1 为什么 `link` 不能直接在目标 inode 上做

硬链接是**目录项**的概念：一个 inode 可以有多个名字，名字属于目录。所以 `link` 必须作用在目录 inode 上（本实验只有根目录），把「新名字 → 已有 inode 号」写进目录的数据块；目标 inode 只增加计数。

### 6.2 锁顺序导致死锁的真实风险

`Inode::modify_disk_inode` 内部会 `get_block_cache(block_id).lock()`。根目录 inode 与普通文件 inode 通常都在 inode 区的相邻位置，很可能落在同一个 512 B 块里。如果在持有目录块缓存锁的情况下调用目标 inode 的 `modify_disk_inode`，同一个 `Mutex` 会被递归加锁而死锁。本实现把「改 nlink」和「改目录项」拆成两次独立的加锁顺序，规避了这一点。

### 6.3 为什么 ch6 的 `spawn` 必须换实现

ch5 的用户程序被 `link_app.S` 链接进内核镜像，可以用 `loader::get_app_data_by_name` 拿到 `&'static [u8]`。ch6 把这些程序变成了 easy-fs 里的普通文件，内核镜像里没有它们，只能 `open_file` + `read_all` 读到 `Vec<u8>` 再解析 ELF。这说明 `spawn` 的接口不变，但"程序从哪里来"随章节演进。

### 6.4 `unlink` 后不回收 inode/数据块

测试注释明确写了「不删除 inode 和数据块也不算错」。本实现只减 `nlink`、删目录项，`nlink` 归零后不调用 `dealloc_data`/回收 inode。真正工程化的实现应在 `nlink == 0` 时清空数据块并释放 inode 位图，但要注意此时可能仍有进程持有该 inode 的 `Arc`（"删除后仍可读"的语义）。

## 7. 不足与后续

- 目录项删除后目录块可能空置，块未回收；反复创建/删除会让目录区缓慢膨胀。
- 没有实现「打开中文件被 unlink 后仍可访问」的完整语义（本实验只要求通过另一个链接名访问）。
- `link` 不支持跨目录、也不检查目标是否为目录（真实系统中目录硬链接是被禁止的）。
