const MAXIMUM_REAL_TIME_TASKS: usize = 64;
const UTILIZATION_ONE: u64 = 1_u64 << 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RealTimeTask {
    pub pid: u32,
    pub computation_time_ns: u64,
    pub deadline_period_ns: u64,
}

impl RealTimeTask {
    const EMPTY: Self = Self {
        pid: 0,
        computation_time_ns: 0,
        deadline_period_ns: 0,
    };
}

#[derive(Clone, Copy)]
struct TaskSlot {
    task: RealTimeTask,
    next_release_ns: u64,
    absolute_deadline_ns: u64,
    remaining_budget_ns: u64,
    active: bool,
}

impl TaskSlot {
    const EMPTY: Self = Self {
        task: RealTimeTask::EMPTY,
        next_release_ns: 0,
        absolute_deadline_ns: 0,
        remaining_budget_ns: 0,
        active: false,
    };
}

pub struct EdfScheduler {
    tasks: [TaskSlot; MAXIMUM_REAL_TIME_TASKS],
    utilization_q32: u64,
}

impl EdfScheduler {
    pub const fn new() -> Self {
        Self {
            tasks: [TaskSlot::EMPTY; MAXIMUM_REAL_TIME_TASKS],
            utilization_q32: 0,
        }
    }

    /// Admits an independent, preemptible, implicit-deadline periodic task.
    ///
    /// The utilization test is conservative Q32.32 arithmetic. Platform code
    /// must reserve additional capacity for interrupt and scheduler overhead.
    pub fn admit_task(
        &mut self,
        task: RealTimeTask,
        first_release_ns: u64,
    ) -> Result<(), SchedulerError> {
        if task.pid == 0
            || task.computation_time_ns == 0
            || task.deadline_period_ns == 0
            || task.computation_time_ns > task.deadline_period_ns
        {
            return Err(SchedulerError::InvalidTask);
        }
        if self
            .tasks
            .iter()
            .any(|slot| slot.active && slot.task.pid == task.pid)
        {
            return Err(SchedulerError::DuplicateTask);
        }
        let utilization = utilization_q32(task)?;
        let admitted = self
            .utilization_q32
            .checked_add(utilization)
            .filter(|total| *total <= UTILIZATION_ONE)
            .ok_or(SchedulerError::Unschedulable)?;
        let slot = self
            .tasks
            .iter_mut()
            .find(|slot| !slot.active)
            .ok_or(SchedulerError::CapacityExceeded)?;
        *slot = TaskSlot {
            task,
            next_release_ns: first_release_ns,
            absolute_deadline_ns: 0,
            remaining_budget_ns: 0,
            active: true,
        };
        self.utilization_q32 = admitted;
        Ok(())
    }

    pub fn next_task(&mut self, current_time_ns: u64) -> Result<Option<u32>, SchedulerError> {
        self.release_due_tasks(current_time_ns)?;
        Ok(self
            .tasks
            .iter()
            .filter(|slot| slot.active && slot.remaining_budget_ns != 0)
            .min_by_key(|slot| slot.absolute_deadline_ns)
            .map(|slot| slot.task.pid))
    }

    pub fn account_runtime(
        &mut self,
        pid: u32,
        elapsed_ns: u64,
        current_time_ns: u64,
    ) -> Result<(), SchedulerError> {
        let slot = self
            .tasks
            .iter_mut()
            .find(|slot| slot.active && slot.task.pid == pid)
            .ok_or(SchedulerError::TaskNotFound)?;
        if elapsed_ns > slot.remaining_budget_ns {
            return Err(SchedulerError::BudgetExceeded(pid));
        }
        slot.remaining_budget_ns -= elapsed_ns;
        if slot.remaining_budget_ns != 0 && current_time_ns > slot.absolute_deadline_ns {
            return Err(SchedulerError::DeadlineMissed(pid));
        }
        Ok(())
    }

    pub const fn utilization_q32(&self) -> u64 {
        self.utilization_q32
    }

    fn release_due_tasks(&mut self, current_time_ns: u64) -> Result<(), SchedulerError> {
        for slot in self.tasks.iter_mut().filter(|slot| slot.active) {
            if slot.remaining_budget_ns != 0 && current_time_ns > slot.absolute_deadline_ns {
                return Err(SchedulerError::DeadlineMissed(slot.task.pid));
            }
            if current_time_ns < slot.next_release_ns {
                continue;
            }
            if slot.remaining_budget_ns != 0 {
                return Err(SchedulerError::DeadlineMissed(slot.task.pid));
            }
            let periods_late =
                (current_time_ns - slot.next_release_ns) / slot.task.deadline_period_ns;
            if periods_late != 0 {
                return Err(SchedulerError::DeadlineMissed(slot.task.pid));
            }
            slot.absolute_deadline_ns = slot
                .next_release_ns
                .checked_add(slot.task.deadline_period_ns)
                .ok_or(SchedulerError::TimeOverflow)?;
            slot.next_release_ns = slot.absolute_deadline_ns;
            slot.remaining_budget_ns = slot.task.computation_time_ns;
        }
        Ok(())
    }
}

impl Default for EdfScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulerError {
    InvalidTask,
    DuplicateTask,
    Unschedulable,
    CapacityExceeded,
    TaskNotFound,
    BudgetExceeded(u32),
    DeadlineMissed(u32),
    TimeOverflow,
}

fn utilization_q32(task: RealTimeTask) -> Result<u64, SchedulerError> {
    let numerator = u128::from(task.computation_time_ns) << 32;
    let denominator = u128::from(task.deadline_period_ns);
    let rounded_up = numerator
        .checked_add(denominator - 1)
        .ok_or(SchedulerError::Unschedulable)?
        / denominator;
    u64::try_from(rounded_up).map_err(|_| SchedulerError::Unschedulable)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(pid: u32, computation: u64, period: u64) -> RealTimeTask {
        RealTimeTask {
            pid,
            computation_time_ns: computation,
            deadline_period_ns: period,
        }
    }

    #[test]
    fn admits_a_schedulable_implicit_deadline_task_set() {
        let mut scheduler = EdfScheduler::new();
        scheduler.admit_task(task(1, 2, 10), 0).unwrap();
        scheduler.admit_task(task(2, 3, 10), 0).unwrap();
        assert_eq!(scheduler.next_task(0), Ok(Some(1)));
        scheduler.account_runtime(1, 2, 2).unwrap();
        assert_eq!(scheduler.next_task(2), Ok(Some(2)));
    }

    #[test]
    fn rejects_utilization_above_one() {
        let mut scheduler = EdfScheduler::new();
        scheduler.admit_task(task(1, 3, 4), 0).unwrap();
        assert_eq!(
            scheduler.admit_task(task(2, 2, 4), 0),
            Err(SchedulerError::Unschedulable)
        );
    }

    #[test]
    fn reports_budget_and_deadline_violations() {
        let mut scheduler = EdfScheduler::new();
        scheduler.admit_task(task(4, 2, 10), 0).unwrap();
        assert_eq!(scheduler.next_task(0), Ok(Some(4)));
        assert_eq!(
            scheduler.account_runtime(4, 3, 1),
            Err(SchedulerError::BudgetExceeded(4))
        );
        assert_eq!(
            scheduler.next_task(11),
            Err(SchedulerError::DeadlineMissed(4))
        );
    }
}
