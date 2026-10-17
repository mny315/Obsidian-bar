use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::OnceLock,
};

use gtk::glib;

type BackgroundJob = Box<dyn FnOnce() + Send + 'static>;

const BACKGROUND_QUEUE_CAPACITY: usize = 128;
const MAX_BACKGROUND_WORKERS: usize = 4;

fn background_queue() -> &'static async_channel::Sender<BackgroundJob> {
    static QUEUE: OnceLock<async_channel::Sender<BackgroundJob>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let (sender, receiver) = async_channel::bounded::<BackgroundJob>(BACKGROUND_QUEUE_CAPACITY);
        let worker_count = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(2)
            .clamp(2, MAX_BACKGROUND_WORKERS);

        for worker_index in 0..worker_count {
            let receiver = receiver.clone();
            let spawn_result = std::thread::Builder::new()
                .name(format!("obsidian-worker-{worker_index}"))
                .spawn(move || {
                    while let Ok(job) = receiver.recv_blocking() {
                        if catch_unwind(AssertUnwindSafe(job)).is_err() {
                            tracing::error!(
                                worker_index,
                                "background job panicked; worker kept alive"
                            );
                        }
                    }
                });
            if let Err(error) = spawn_result {
                tracing::error!(%error, worker_index, "failed to start background worker");
            }
        }

        sender
    })
}

fn background_receiver<T, Job>(job: Job) -> async_channel::Receiver<T>
where
    T: Send + 'static,
    Job: FnOnce() -> T + Send + 'static,
{
    let (sender, receiver) = async_channel::bounded::<T>(1);
    let task: BackgroundJob = Box::new(move || {
        if !sender.is_closed() {
            let _ = sender.send_blocking(job());
        }
    });

    let queue = background_queue();
    match queue.try_send(task) {
        Ok(()) => {}
        Err(async_channel::TrySendError::Full(task)) => {
            let queue = queue.clone();
            glib::MainContext::default().spawn_local(async move {
                if queue.send(task).await.is_err() {
                    tracing::error!("background worker queue is unavailable; job was cancelled");
                }
            });
        }
        Err(async_channel::TrySendError::Closed(_)) => {
            tracing::error!("background worker queue is unavailable; job was cancelled");
        }
    }
    receiver
}

pub(super) fn run_background<T, Job, Done>(job: Job, done: Done)
where
    T: Send + 'static,
    Job: FnOnce() -> T + Send + 'static,
    Done: FnOnce(T) + 'static,
{
    let receiver = background_receiver(job);
    glib::MainContext::default().spawn_local(async move {
        if let Ok(result) = receiver.recv().await {
            done(result);
        }
    });
}

pub(super) async fn run_background_async<T, Job>(job: Job) -> Option<T>
where
    T: Send + 'static,
    Job: FnOnce() -> T + Send + 'static,
{
    let receiver = background_receiver(job);
    receiver.recv().await.ok()
}
