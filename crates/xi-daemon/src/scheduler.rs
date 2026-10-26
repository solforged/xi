//! FIFO, per-endpoint model request leases.

use std::collections::{BTreeMap, HashMap, VecDeque};

use xi_kernel::config::Limit;
use xi_protocol::methods::client::LeaseStatus;

pub(crate) struct Scheduler<T> {
    endpoints: BTreeMap<String, Endpoint<T>>,
}

struct Endpoint<T> {
    concurrency: usize,
    active: HashMap<u64, usize>,
    waiting: VecDeque<(u64, T)>,
}

impl<T> Scheduler<T> {
    pub(crate) fn new(limits: &[Limit]) -> Self {
        Self {
            endpoints: limits
                .iter()
                .map(|limit| {
                    (
                        limit.endpoint.clone(),
                        Endpoint {
                            concurrency: limit.concurrency,
                            active: HashMap::new(),
                            waiting: VecDeque::new(),
                        },
                    )
                })
                .collect(),
        }
    }

    pub(crate) fn acquire(&mut self, worker: u64, endpoint: String, request: T) -> Vec<T> {
        let state = self.endpoints.entry(endpoint).or_insert_with(|| Endpoint {
            concurrency: usize::MAX,
            active: HashMap::new(),
            waiting: VecDeque::new(),
        });
        state.waiting.push_back((worker, request));
        state.grant()
    }

    pub(crate) fn release(&mut self, worker: u64, endpoint: &str) -> Vec<T> {
        let Some(state) = self.endpoints.get_mut(endpoint) else {
            return Vec::new();
        };
        if let Some(count) = state.active.get_mut(&worker) {
            *count -= 1;
            if *count == 0 {
                state.active.remove(&worker);
            }
        }
        state.grant()
    }

    pub(crate) fn remove_worker(&mut self, worker: u64) -> Vec<T> {
        let mut granted = Vec::new();
        for state in self.endpoints.values_mut() {
            state.active.remove(&worker);
            state.waiting.retain(|(owner, _)| *owner != worker);
            granted.extend(state.grant());
        }
        granted
    }

    pub(crate) fn status(&self) -> Vec<LeaseStatus> {
        self.endpoints
            .iter()
            .map(|(endpoint, state)| LeaseStatus {
                endpoint: endpoint.clone(),
                concurrency: state.concurrency,
                active: state.active.values().sum(),
                waiting: state.waiting.len(),
            })
            .collect()
    }
}

impl<T> Endpoint<T> {
    fn grant(&mut self) -> Vec<T> {
        let mut granted = Vec::new();
        let mut active: usize = self.active.values().sum();
        while active < self.concurrency {
            let Some((worker, request)) = self.waiting.pop_front() else {
                break;
            };
            *self.active.entry(worker).or_default() += 1;
            granted.push(request);
            active += 1;
        }
        granted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_fifo_at_concurrency_one() {
        let mut scheduler = Scheduler::new(&[Limit {
            endpoint: "local".into(),
            concurrency: 1,
        }]);
        assert_eq!(scheduler.acquire(1, "local".into(), "first"), vec!["first"]);
        assert!(scheduler.acquire(2, "local".into(), "second").is_empty());
        assert!(scheduler.acquire(3, "local".into(), "third").is_empty());
        assert_eq!(scheduler.status()[0].active, 1);
        assert_eq!(scheduler.status()[0].waiting, 2);
        assert_eq!(scheduler.release(1, "local"), vec!["second"]);
        assert_eq!(scheduler.release(2, "local"), vec!["third"]);
        assert!(scheduler.release(3, "local").is_empty());
        assert_eq!(scheduler.status()[0].active, 0);
    }

    #[test]
    fn exit_releases_all_leases_and_removes_waiters() {
        let mut scheduler = Scheduler::new(&[Limit {
            endpoint: "local".into(),
            concurrency: 1,
        }]);
        assert_eq!(scheduler.acquire(1, "local".into(), 10), vec![10]);
        assert!(scheduler.acquire(1, "local".into(), 11).is_empty());
        assert!(scheduler.acquire(2, "local".into(), 20).is_empty());
        assert_eq!(scheduler.remove_worker(1), vec![20]);
        assert_eq!(scheduler.status()[0].waiting, 0);
    }

    #[test]
    fn unlimited_endpoints_grant_without_waiting() {
        let mut scheduler = Scheduler::new(&[]);
        assert_eq!(scheduler.acquire(1, "remote".into(), 1), vec![1]);
        assert_eq!(scheduler.acquire(2, "remote".into(), 2), vec![2]);
        assert_eq!(scheduler.status()[0].active, 2);
        assert!(scheduler.remove_worker(1).is_empty());
        assert_eq!(scheduler.status()[0].active, 1);
    }
}
