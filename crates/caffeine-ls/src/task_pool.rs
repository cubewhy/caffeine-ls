//! A minimal fixed-size worker pool whose threads carry a large stack.
//!
//! Recursive analysis over real-world Java needs far more than the default
//! thread stack: generated sources (ANTLR parsers, protobuf) hold initializer
//! chains hundreds of nodes deep and the parser, body lowering and type
//! inference each recurse per node. `threadpool`'s workers run on the platform
//! default (2 MiB on Linux) and overflow on such input; these workers get an
//! explicit [`TASK_STACK_SIZE`].

use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

/// Stack per worker thread — the same headroom the main loop thread gets.
pub(crate) const TASK_STACK_SIZE: usize = 16 * 1024 * 1024;

type Job = Box<dyn FnOnce() + Send + 'static>;

pub(crate) struct TaskPool {
    sender: Option<mpsc::Sender<Job>>,
    workers: Vec<JoinHandle<()>>,
}

impl TaskPool {
    pub(crate) fn new(name: &str, size: usize) -> Self {
        let (sender, receiver) = mpsc::channel::<Job>();
        let receiver = Arc::new(Mutex::new(receiver));
        let workers = (0..size)
            .map(|i| {
                std::thread::Builder::new()
                    .name(format!("{name}-{i}"))
                    .stack_size(TASK_STACK_SIZE)
                    .spawn({
                        let receiver = Arc::clone(&receiver);
                        move || loop {
                            let job = receiver.lock().unwrap().recv();
                            match job {
                                Ok(job) => job(),
                                Err(_) => break,
                            }
                        }
                    })
                    .expect("failed to spawn task-pool worker")
            })
            .collect();
        Self {
            sender: Some(sender),
            workers,
        }
    }

    pub(crate) fn execute(&self, job: impl FnOnce() + Send + 'static) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(Box::new(job));
        }
    }
}

impl Drop for TaskPool {
    fn drop(&mut self) {
        self.sender = None;
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}
