use std::sync::{Arc, Mutex, OnceLock, mpsc};

type Job = Box<dyn FnOnce() + Send>;

struct Workers {
    sender: mpsc::SyncSender<Job>,
}

impl Workers {
    fn new() -> Option<Self> {
        let count = std::thread::available_parallelism()
            .map_or(4, usize::from)
            .clamp(1, 16);
        let (sender, receiver) = mpsc::sync_channel::<Job>(count * 2);
        let receiver = Arc::new(Mutex::new(receiver));
        let mut started = 0;
        for index in 0..count {
            let receiver = receiver.clone();
            match std::thread::Builder::new()
                .name(format!("nohrs-name-scan-{index}"))
                .spawn(move || {
                    loop {
                        let next = receiver
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .recv();
                        match next {
                            Ok(job) => job(),
                            Err(_) => return,
                        }
                    }
                }) {
                Ok(_) => started += 1,
                Err(error) => tracing::warn!(%error, "could not start a search worker"),
            }
        }
        (started > 0).then_some(Self { sender })
    }
}

pub(super) fn submit(job: impl FnOnce() + Send + 'static) {
    static WORKERS: OnceLock<Option<Workers>> = OnceLock::new();
    let job: Job = Box::new(job);
    match WORKERS.get_or_init(Workers::new) {
        Some(workers) => {
            if let Err(error) = workers.sender.send(job) {
                tracing::warn!("search workers stopped; scanning on the caller");
                error.0();
            }
        }
        None => job(),
    }
}
