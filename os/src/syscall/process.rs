//! Process management syscalls
use crate::config::PAGE_SIZE;
use crate::mm::{
    translated_byte_buffer_try, MapPermission, PageTable, VirtAddr,
};
use crate::task::{
    change_program_brk, current_user_token, exit_current_and_run_next, get_syscall_count, mmap,
    munmap, suspend_current_and_run_next,
};
use crate::timer::get_time_us;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(_exit_code: i32) -> ! {
    trace!("kernel: sys_exit");
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// get time with second and microsecond
///
/// `TimeVal` is 16 bytes and may straddle two pages, so write it through the
/// page table buffer by buffer instead of one plain `*ts = ...`.
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
    let us = get_time_us();
    let time_val = TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    };
    let src = unsafe {
        core::slice::from_raw_parts(
            &time_val as *const TimeVal as *const u8,
            core::mem::size_of::<TimeVal>(),
        )
    };
    match translated_byte_buffer_try(current_user_token(), ts as *const u8, src.len()) {
        Some(buffers) => {
            let mut copied = 0;
            for buffer in buffers {
                let n = buffer.len().min(src.len() - copied);
                buffer[..n].copy_from_slice(&src[copied..copied + n]);
                copied += n;
            }
            0
        }
        None => -1,
    }
}

/// Translate one user byte to a kernel-writable pointer plus its permissions.
fn user_byte(token: usize, addr: usize) -> Option<(*mut u8, bool, bool)> {
    // SV39 user space is [0, 1 << 38); anything above belongs to the kernel
    // (trampoline / trap context) and must not be reachable via trace.
    const MAX_USER_VA: usize = 1 << 38;
    if addr >= MAX_USER_VA {
        return None;
    }
    let page_table = PageTable::from_token(token);
    let va = VirtAddr::from(addr);
    let pte = page_table.translate(va.floor())?;
    if !pte.is_valid() {
        return None;
    }
    let ptr = pte
        .ppn()
        .get_bytes_array()
        .as_mut_ptr()
        .wrapping_add(va.page_offset());
    Some((ptr, pte.readable(), pte.writable()))
}

/// trace syscall: 0 = read one user byte, 1 = write one user byte,
/// 2 = count how many times this task has called syscall `id`.
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");
    let token = current_user_token();
    match trace_request {
        0 => match user_byte(token, id) {
            Some((ptr, readable, _)) if readable => unsafe { ptr.read_volatile() as isize },
            _ => -1,
        },
        1 => match user_byte(token, id) {
            Some((ptr, _, writable)) if writable => {
                unsafe { ptr.write_volatile(data as u8) };
                0
            }
            _ => -1,
        },
        2 => get_syscall_count(id),
        _ => -1,
    }
}

/// mmap: map `len` bytes at `start` with `port` (bit0 R, bit1 W, bit2 X).
pub fn sys_mmap(start: usize, len: usize, port: usize) -> isize {
    trace!("kernel: sys_mmap");
    if len == 0 || port == 0 || port & !0b111 != 0 {
        return -1;
    }
    let start_va = VirtAddr::from(start);
    if !start_va.aligned() {
        return -1;
    }
    let end = match start.checked_add(len) {
        Some(end) => end,
        None => return -1,
    };
    let mut permission = MapPermission::U;
    if port & 0b001 != 0 {
        permission |= MapPermission::R;
    }
    if port & 0b010 != 0 {
        permission |= MapPermission::W;
    }
    if port & 0b100 != 0 {
        permission |= MapPermission::X;
    }
    if mmap(start_va, VirtAddr::from(end), permission) {
        0
    } else {
        -1
    }
}

/// munmap: unmap a page-aligned range that must match a whole mapped area.
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel: sys_munmap");
    let start_va = VirtAddr::from(start);
    if !start_va.aligned() || len == 0 || len % PAGE_SIZE != 0 {
        return -1;
    }
    let end = match start.checked_add(len) {
        Some(end) => end,
        None => return -1,
    };
    if munmap(start_va, VirtAddr::from(end)) {
        0
    } else {
        -1
    }
}
/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel: sys_sbrk");
    if let Some(old_brk) = change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}
