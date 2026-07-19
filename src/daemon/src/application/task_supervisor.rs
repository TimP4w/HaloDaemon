// SPDX-License-Identifier: GPL-3.0-or-later
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;

use crate::application::state::AppState;

pub(crate) struct TaskHandle(pub tokio::task::JoinHandle<()>);

impl Drop for TaskHandle {
    fn drop(&mut self) {
        // Tokio detaches a bare JoinHandle on drop. Supervised work must never
        // outlive its manager, including when the manager itself is aborted.
        self.0.abort();
    }
}

type StartFuture = Pin<Box<dyn Future<Output = TaskHandle> + Send>>;
type StartTask = Arc<dyn Fn() -> StartFuture + Send + Sync>;
type StopTask = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug, Clone)]
pub enum TaskState {
    NotStarted,
    Running(tokio::task::AbortHandle),
    Restarting,
    Stopping,
    Stopped,
    Failed(String),
}

impl TaskState {
    fn summary(&self) -> String {
        match self {
            Self::NotStarted => "NotStarted".to_owned(),
            Self::Running(handle) => format!("Running({:?})", handle.id()),
            Self::Restarting => "Restarting".to_owned(),
            Self::Stopping => "Stopping".to_owned(),
            Self::Stopped => "Stopped".to_owned(),
            Self::Failed(reason) => format!("Failed({reason})"),
        }
    }
}

struct ManagedTask {
    state: Arc<Mutex<TaskState>>,
    shutdown_tx: watch::Sender<bool>,
    manager: tokio::task::JoinHandle<()>,
}

pub struct TaskSupervisor {
    app: Arc<AppState>,
    tasks: Vec<ManagedTask>,
}

impl TaskSupervisor {
    pub fn new(app: Arc<AppState>) -> Self {
        Self {
            app,
            tasks: Vec::new(),
        }
    }

    pub fn register(
        &mut self,
        name: &'static str,
        failure_detail: &'static str,
        start: impl Fn() -> StartFuture + Send + Sync + 'static,
        stop: impl Fn() + Send + Sync + 'static,
    ) -> Arc<Mutex<TaskState>> {
        let state = Arc::new(Mutex::new(TaskState::NotStarted));
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let start: StartTask = Arc::new(start);
        let stop: StopTask = Arc::new(stop);
        let manager_state = Arc::clone(&state);
        let app = Arc::clone(&self.app);
        let manager = tokio::spawn(async move {
            let mut failures = 0u32;
            loop {
                if *shutdown_rx.borrow() {
                    set_state(&manager_state, TaskState::Stopped);
                    return;
                }

                let mut start_future = start();
                let mut child = tokio::select! {
                    child = &mut start_future => child,
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            set_state(&manager_state, TaskState::Stopped);
                            return;
                        }
                        continue;
                    }
                };
                set_state(&manager_state, TaskState::Running(child.0.abort_handle()));

                let result = tokio::select! {
                    result = &mut child.0 => Some(result),
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            set_state(&manager_state, TaskState::Stopping);
                            stop();
                            if tokio::time::timeout(Duration::from_secs(2), &mut child.0).await.is_err() {
                                child.0.abort();
                                let _ = (&mut child.0).await;
                            }
                            set_state(&manager_state, TaskState::Stopped);
                            return;
                        }
                        None
                    }
                };
                let Some(result) = result else { continue };

                let reason = match result {
                    Ok(()) => "exited unexpectedly".to_owned(),
                    Err(error) if error.is_panic() => format!("panicked: {error}"),
                    Err(error) => format!("was cancelled unexpectedly: {error}"),
                };
                failures += 1;
                set_state(&manager_state, TaskState::Failed(reason.clone()));
                log::error!("{name} {reason}");
                if !failure_detail.is_empty() {
                    crate::infrastructure::platform::notify::send(
                        &app,
                        halod_shared::types::NotificationCode::EngineStopped {
                            detail: failure_detail.to_owned(),
                        },
                    )
                    .await;
                }

                if failures >= 3 {
                    return;
                }
                set_state(&manager_state, TaskState::Restarting);
                let delay = Duration::from_secs(1 << (failures - 1));
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            set_state(&manager_state, TaskState::Stopped);
                            return;
                        }
                    }
                }
            }
        });
        self.tasks.push(ManagedTask {
            state: Arc::clone(&state),
            shutdown_tx,
            manager,
        });
        state
    }

    pub async fn shutdown(self) {
        for task in &self.tasks {
            task.shutdown_tx.send_replace(true);
        }
        for mut task in self.tasks {
            if tokio::time::timeout(Duration::from_secs(3), &mut task.manager)
                .await
                .is_err()
            {
                task.manager.abort();
                let _ = task.manager.await;
                set_state(
                    &task.state,
                    TaskState::Failed("supervisor timeout".to_owned()),
                );
            }
        }
    }
}

fn set_state(state: &Mutex<TaskState>, next: TaskState) {
    log::trace!("task supervisor transition -> {}", next.summary());
    *state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = next;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(start_paused = true)]
    async fn failed_task_restarts_and_shutdown_joins_it() {
        let app = Arc::new(AppState::new(crate::config::Config::default()));
        let starts = Arc::new(AtomicUsize::new(0));
        let mut supervisor = TaskSupervisor::new(app);
        let starts_for_task = Arc::clone(&starts);
        let state = supervisor.register(
            "test",
            "",
            move || {
                let starts = Arc::clone(&starts_for_task);
                Box::pin(async move {
                    starts.fetch_add(1, Ordering::SeqCst);
                    TaskHandle(tokio::spawn(async {}))
                })
            },
            || {},
        );

        for _ in 0..20 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
            if starts.load(Ordering::SeqCst) >= 2 {
                break;
            }
        }
        assert!(starts.load(Ordering::SeqCst) >= 2);
        supervisor.shutdown().await;
        assert!(matches!(*state.lock().unwrap(), TaskState::Stopped));
    }

    #[tokio::test]
    async fn dropping_a_supervised_handle_aborts_instead_of_detaching() {
        let child = tokio::spawn(std::future::pending::<()>());
        let abort = child.abort_handle();
        drop(TaskHandle(child));
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
    }

    #[tokio::test]
    async fn shutdown_cancels_a_start_that_never_completes() {
        let app = Arc::new(AppState::new(crate::config::Config::default()));
        let mut supervisor = TaskSupervisor::new(app);
        let state = supervisor.register(
            "starting test",
            "",
            || Box::pin(std::future::pending::<TaskHandle>()),
            || {},
        );
        tokio::task::yield_now().await;
        supervisor.shutdown().await;
        assert!(matches!(*state.lock().unwrap(), TaskState::Stopped));
    }
}
