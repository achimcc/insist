//! The long-running loops beside the HTTP server: reconciliation, the tick,
//! the acknowledgement stream. None of them is meant to end. One that
//! returns or panics would leave insist answering `/health` and `/metrics`
//! while it no longer reconciles, escalates or reads presses — alive to
//! every probe and useless. So the first one to end ends the process with
//! an error, and systemd restarts it.
use std::collections::HashMap;
use std::future::{Future, IntoFuture};
use tokio::task::{Id, JoinSet};

#[derive(Default)]
pub struct Tasks {
    set: JoinSet<()>,
    names: HashMap<Id, &'static str>,
}

impl Tasks {
    pub fn spawn<F>(&mut self, name: &'static str, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let handle = self.set.spawn(task);
        self.names.insert(handle.id(), name);
    }

    /// Waits for the first task to end and says which one and how.
    async fn first_to_end(&mut self) -> anyhow::Error {
        let (id, how) = match self.set.join_next_with_id().await {
            None => return anyhow::anyhow!("no background task is running"),
            Some(Ok((id, ()))) => (id, "returned"),
            Some(Err(e)) if e.is_panic() => (e.id(), "panicked"),
            Some(Err(e)) => (e.id(), "was cancelled"),
        };
        let name = self.names.get(&id).copied().unwrap_or("unnamed");
        anyhow::anyhow!("the {name} task {how}; exiting so the service manager restarts insist")
    }
}

/// Runs `server` while supervising `tasks`. A server that shuts down cleanly
/// is `Ok`; a server error or any task ending is `Err`, naming the task.
pub async fn run_tasks<S>(mut tasks: Tasks, server: S) -> anyhow::Result<()>
where
    S: IntoFuture<Output = std::io::Result<()>>,
{
    tokio::select! {
        served = server.into_future() => Ok(served?),
        ended = tasks.first_to_end() => {
            tracing::error!("{ended}");
            Err(ended)
        }
    }
}
