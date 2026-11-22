//!Implementation of [`TaskManager`]
use super::TaskControlBlock;
use crate::sync::UPSafeCell;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use lazy_static::*;
///A array of `TaskControlBlock` that is thread-safe
pub struct TaskManager {
    ready_queue: VecDeque<Arc<TaskControlBlock>>,
}

/// A stride scheduler: always pick the task with the smallest `pass`.
impl TaskManager {
    ///Creat an empty TaskManager
    pub fn new() -> Self {
        Self {
            ready_queue: VecDeque::new(),
        }
    }
    /// Add process back to ready queue
    pub fn add(&mut self, task: Arc<TaskControlBlock>) {
        // A fresh process should not be able to starve the others: start it
        // from the current minimum pass instead of 0.
        let min_pass = self
            .ready_queue
            .iter()
            .map(|t| t.inner_exclusive_access().pass)
            .min();
        if let Some(min_pass) = min_pass {
            let mut inner = task.inner_exclusive_access();
            if inner.pass < min_pass {
                inner.pass = min_pass;
            }
        }
        self.ready_queue.push_back(task);
    }
    /// Take the process with the smallest pass out of the ready queue
    pub fn fetch(&mut self) -> Option<Arc<TaskControlBlock>> {
        let mut selected: Option<usize> = None;
        let mut min_pass = usize::MAX;
        for (index, task) in self.ready_queue.iter().enumerate() {
            let pass = task.inner_exclusive_access().pass;
            if pass < min_pass {
                min_pass = pass;
                selected = Some(index);
            }
        }
        let index = selected?;
        let task = self.ready_queue.remove(index)?;
        // Charge one stride to the task which is about to run.
        {
            let mut inner = task.inner_exclusive_access();
            inner.pass += inner.stride;
        }
        Some(task)
    }
}

lazy_static! {
    /// TASK_MANAGER instance through lazy_static!
    pub static ref TASK_MANAGER: UPSafeCell<TaskManager> =
        unsafe { UPSafeCell::new(TaskManager::new()) };
}

/// Add process to ready queue
pub fn add_task(task: Arc<TaskControlBlock>) {
    //trace!("kernel: TaskManager::add_task");
    TASK_MANAGER.exclusive_access().add(task);
}

/// Take a process out of the ready queue
pub fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    //trace!("kernel: TaskManager::fetch_task");
    TASK_MANAGER.exclusive_access().fetch()
}
