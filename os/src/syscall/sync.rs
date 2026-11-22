use crate::sync::{Condvar, Mutex, MutexBlocking, MutexSpin, Semaphore};
use crate::task::{
    block_current_and_run_next, current_process, current_task, ProcessControlBlock, TaskStatus,
};
use crate::timer::{add_timer, get_time_ms};
use alloc::sync::Arc;
use alloc::{vec, vec::Vec};
/// sleep syscall
pub fn sys_sleep(ms: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_sleep",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let expire_ms = get_time_ms() + ms;
    let task = current_task().unwrap();
    add_timer(expire_ms, task);
    block_current_and_run_next();
    0
}
/// mutex create syscall
pub fn sys_mutex_create(blocking: bool) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mutex: Option<Arc<dyn Mutex>> = if !blocking {
        Some(Arc::new(MutexSpin::new()))
    } else {
        Some(Arc::new(MutexBlocking::new()))
    };
    let mut process_inner = process.inner_exclusive_access();
    if let Some(id) = process_inner
        .mutex_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.mutex_list[id] = mutex;
        id as isize
    } else {
        process_inner.mutex_list.push(mutex);
        process_inner.mutex_list.len() as isize - 1
    }
}
/// mutex lock syscall
pub fn sys_mutex_lock(mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_lock",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let task = current_task().unwrap();
    let process_inner = process.inner_exclusive_access();
    let detect = process_inner.deadlock_detect;
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    drop(process_inner);
    drop(process);
    if detect && task.inner_exclusive_access().mutex_held.contains(&mutex_id) {
        // Locking a non-recursive mutex twice by the same thread deadlocks itself.
        return -0xdead;
    }
    mutex.lock();
    task.inner_exclusive_access().mutex_held.push(mutex_id);
    0
}
/// mutex unlock syscall
pub fn sys_mutex_unlock(mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_unlock",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let task = current_task().unwrap();
    let process_inner = process.inner_exclusive_access();
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    drop(process_inner);
    drop(process);
    mutex.unlock();
    task.inner_exclusive_access()
        .mutex_held
        .retain(|held| *held != mutex_id);
    0
}
/// semaphore create syscall
pub fn sys_semaphore_create(res_count: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let id = if let Some(id) = process_inner
        .semaphore_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.semaphore_list[id] = Some(Arc::new(Semaphore::new(res_count)));
        id
    } else {
        process_inner
            .semaphore_list
            .push(Some(Arc::new(Semaphore::new(res_count))));
        process_inner.semaphore_list.len() - 1
    };
    id as isize
}
/// semaphore up syscall
pub fn sys_semaphore_up(sem_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_up",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let task = current_task().unwrap();
    let process_inner = process.inner_exclusive_access();
    let sem = Arc::clone(process_inner.semaphore_list[sem_id].as_ref().unwrap());
    drop(process_inner);
    {
        let mut task_inner = task.inner_exclusive_access();
        if sem_id < task_inner.sem_allocated.len() && task_inner.sem_allocated[sem_id] > 0 {
            task_inner.sem_allocated[sem_id] -= 1;
        }
    }
    drop(process);
    sem.up();
    0
}
/// semaphore down syscall
pub fn sys_semaphore_down(sem_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_down",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let task = current_task().unwrap();
    let (sem, detect, sem_count) = {
        let process_inner = process.inner_exclusive_access();
        (
            Arc::clone(process_inner.semaphore_list[sem_id].as_ref().unwrap()),
            process_inner.deadlock_detect,
            process_inner.semaphore_list.len(),
        )
    };
    if detect {
        let tid = match task.inner_exclusive_access().res.as_ref() {
            Some(res) => res.tid,
            None => return -1,
        };
        if !sem_request_is_safe(&process, tid, sem_id) {
            return -0xdead;
        }
    }
    task.inner_exclusive_access().sem_waiting = Some(sem_id);
    sem.down();
    {
        let mut task_inner = task.inner_exclusive_access();
        task_inner.sem_waiting = None;
        if task_inner.sem_allocated.len() < sem_count {
            task_inner.sem_allocated.resize(sem_count, 0);
        }
        task_inner.sem_allocated[sem_id] += 1;
    }
    0
}
/// condvar create syscall
pub fn sys_condvar_create() -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let id = if let Some(id) = process_inner
        .condvar_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.condvar_list[id] = Some(Arc::new(Condvar::new()));
        id
    } else {
        process_inner
            .condvar_list
            .push(Some(Arc::new(Condvar::new())));
        process_inner.condvar_list.len() - 1
    };
    id as isize
}
/// condvar signal syscall
pub fn sys_condvar_signal(condvar_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_signal",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let condvar = Arc::clone(process_inner.condvar_list[condvar_id].as_ref().unwrap());
    drop(process_inner);
    condvar.signal();
    0
}
/// condvar wait syscall
pub fn sys_condvar_wait(condvar_id: usize, mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_wait",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let condvar = Arc::clone(process_inner.condvar_list[condvar_id].as_ref().unwrap());
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    drop(process_inner);
    condvar.wait(mutex);
    0
}
/// enable deadlock detection syscall
///
/// YOUR JOB: Implement deadlock detection, but might not all in this syscall
pub fn sys_enable_deadlock_detect(enabled: usize) -> isize {
    trace!("kernel: sys_enable_deadlock_detect");
    let process = current_process();
    process.inner_exclusive_access().deadlock_detect = enabled != 0;
    0
}

/// Banker's-algorithm safety check: would blocking `current_tid` on `sem_id`
/// leave the process in an unsafe (deadlocking) state?
fn sem_request_is_safe(
    process: &Arc<ProcessControlBlock>,
    current_tid: usize,
    sem_id: usize,
) -> bool {
    let process_inner = process.inner_exclusive_access();
    let sem_count = process_inner.semaphore_list.len();
    if sem_id >= sem_count {
        return true;
    }
    // available[j] = free units of semaphore j
    let mut available: Vec<isize> = (0..sem_count)
        .map(|j| match process_inner.semaphore_list[j].as_ref() {
            Some(sem) => sem.available(),
            None => 0,
        })
        .collect();
    // (pending semaphore, allocated units) of every live thread
    let mut threads: Vec<(Option<usize>, Vec<usize>)> = Vec::new();
    for task in process_inner.tasks.iter().flatten() {
        let task_inner = task.inner_exclusive_access();
        let tid = match task_inner.res.as_ref() {
            Some(res) => res.tid,
            None => continue,
        };
        // A thread that was just woken (status Ready/Running) is no longer
        // waiting, even if `sem_waiting` has not been cleared yet.
        let mut pending = if task_inner.task_status == TaskStatus::Blocked {
            task_inner.sem_waiting
        } else {
            None
        };
        if tid == current_tid {
            pending = Some(sem_id);
        }
        threads.push((pending, task_inner.sem_allocated.clone()));
    }
    drop(process_inner);
    if threads.is_empty() {
        return true;
    }
    // Try to find a safe sequence: a thread can finish if its pending request
    // can be satisfied now; finishing releases everything it holds.
    let mut finished = vec![false; threads.len()];
    let mut progress = true;
    while progress {
        progress = false;
        for (index, (pending, allocated)) in threads.iter().enumerate() {
            if finished[index] {
                continue;
            }
            let can_finish = match pending {
                None => true,
                Some(j) => *j < available.len() && available[*j] >= 1,
            };
            if !can_finish {
                continue;
            }
            if let Some(j) = pending {
                available[*j] -= 1;
            }
            for (j, held) in allocated.iter().enumerate() {
                if *held > 0 && j < available.len() {
                    available[j] += *held as isize;
                }
            }
            finished[index] = true;
            progress = true;
        }
    }
    finished.iter().all(|done| *done)
}
