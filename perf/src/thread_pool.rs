use std::{
    fmt,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Sender},
    },
};

type Job<'scope, T> = Box<dyn for<'call> FnOnce(&'call mut T) + Send + 'scope>;

enum Message<'scope, T> {
    Job(Job<'scope, T>),
    Stop,
}

pub struct ScopedThreadPool<'scope, T> {
    senders: Vec<Sender<Message<'scope, T>>>,
    next_index: AtomicUsize,
}

impl<'scope, T> ScopedThreadPool<'scope, T>
where
    T: 'scope,
{
    pub fn spawn<F>(&self, f: F)
    where
        F: for<'call> FnOnce(&'call mut T) + Send + 'scope,
    {
        let job = Box::new(f);
        let index = self.next_index.fetch_add(1, Ordering::Relaxed);
        let sender = &self.senders[index % self.senders.len()];
        sender
            .send(Message::Job(job))
            .expect("failed to send a job through the channel");
    }
}

/// Creates a scoped thread pool with per-thread mutable state.
///
/// All threads and jobs will be joined before this function returns.
pub fn scope_with_states<'scope, N, T, F, R>(name: N, worker_states: &'scope mut [T], f: F) -> R
where
    N: fmt::Display,
    T: Send + 'scope,
    F: FnOnce(&ScopedThreadPool<'scope, T>) -> R,
{
    let (senders, receivers): (Vec<_>, Vec<_>) =
        (0..worker_states.len()).map(|_| mpsc::channel()).collect();

    let pool = ScopedThreadPool {
        senders: senders.clone(),
        next_index: AtomicUsize::new(0),
    };

    std::thread::scope(|s| {
        for (i, (worker_state, receiver)) in worker_states.iter_mut().zip(receivers).enumerate() {
            std::thread::Builder::new()
                .name(format!("{name}{i:02}"))
                .spawn_scoped(s, move || {
                    for msg in receiver {
                        match msg {
                            Message::Job(job) => {
                                let _ = catch_unwind(AssertUnwindSafe(|| job(worker_state)));
                            }
                            Message::Stop => break,
                        }
                    }
                })
                .unwrap();
        }

        let res = f(&pool);

        for (i, sender) in senders.iter().enumerate() {
            sender.send(Message::Stop).unwrap_or_else(|e| {
                panic!("failed to send a `stop` message to the worker {i}: {e}")
            });
        }

        res
    })
}

#[cfg(test)]
mod test {
    use super::*;

    /// Checks whether worker states are actually modified by jobs.
    #[test]
    fn test_thread_pool_state() {
        let mut states: Vec<u32> = vec![u32::default(); num_cpus::get()];

        scope_with_states("foo", &mut states, |s| {
            for _ in 0..10_000 {
                s.spawn(|state| *state = state.saturating_add(1));
            }
        });

        let sum: u32 = states.iter().sum();
        assert_eq!(sum, 10_000);
    }

    /// Checks whether scoped tasks can use immutable references.
    #[test]
    fn test_thread_pool_scope_ref() {
        let jobs = num_cpus::get();
        let mut states: Vec<Vec<u32>> = (0..jobs)
            .map(|_| Vec::with_capacity(10_000 / jobs))
            .collect();
        let v: Vec<_> = (0..10_000).collect();

        scope_with_states("foo", &mut states, |s| {
            for i in v.iter() {
                s.spawn(|state| state.push(*i));
            }
        });

        let mut res: Vec<u32> = states.into_iter().flatten().collect();
        res.sort();
        assert_eq!(v, res);
    }

    /// Checks whether scoped tasks can use mutable references.
    #[test]
    fn test_thread_pool_scope_ref_mut() {
        let mut states: Vec<()> = (0..num_cpus::get()).map(|_| ()).collect();
        let mut v: Vec<u32> = (0..100).collect();

        scope_with_states("foo", &mut states, |s| {
            for i in v.iter_mut() {
                s.spawn(|_| *i = i.saturating_add(1));
            }
        });

        assert_eq!(v, (1..=100).collect::<Vec<u32>>());
    }

    /// Check whether scoped tasks can use moved values.
    #[test]
    fn test_thread_pool_scope_move() {
        let value: u32 = 5;
        let mut states = vec![u32::default(); num_cpus::get()];

        scope_with_states("foo", &mut states, |s| {
            for _ in 0..10 {
                s.spawn(move |state| {
                    *state = state.saturating_add(value);
                });
            }
        });

        let sum: u32 = states.iter().sum();
        assert_eq!(sum, 50);
    }

    /// Tests nested pool usage.
    #[test]
    fn test_thread_pool_nested_pools() {
        let mut outer_states: Vec<u32> = vec![0; num_cpus::get()];
        let mut inner_states: Vec<u32> = vec![0; num_cpus::get()];

        scope_with_states("outer", &mut outer_states, |outer_pool| {
            outer_pool.spawn(|_| {
                scope_with_states("inner", &mut inner_states, |inner_pool| {
                    for _ in 0..5 {
                        inner_pool.spawn(|state| *state = state.saturating_add(1));
                    }
                });
            });
        });

        let inner_sum: u32 = inner_states.iter().sum();
        assert_eq!(inner_sum, 5);
    }
}
