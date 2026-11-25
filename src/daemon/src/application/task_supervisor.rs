// SPDX-License-Identifier: GPL-3.0-or-later
use parking_lot::Mutex;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

use tokio::sync::watch;

use crate::application::notifications;
use crate::application::state::AppState;
use crate::util::backoff::{respawn_wait, FailureStreak};
use crate::util::task::TaskHandle;

const MAX_FAILURES: u32 = 3;
const STABLE_RUN: Duration = Duration::from_secs(60);

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
            let mut streak: Option<FailureStreak> = None;
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
                let started = Instant::now();

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
                if started.elapsed() >= STABLE_RUN {
                    streak = None;
                }
                let streak = FailureStreak::record_in(&mut streak, Instant::now());
                set_state(&manager_state, TaskState::Failed(reason.clone()));
                log::error!("{name} {reason}");
                if !failure_detail.is_empty() {
                    notifications::send(
                        &app,
                        halod_shared::types::NotificationCode::EngineStopped {
                            detail: failure_detail.to_owned(),
                        },
                    )
                    .await;
                }

                if streak.failures() >= MAX_FAILURES {
                    log::error!("{name} failed {MAX_FAILURES} times in a row; not restarting it");
                    return;
                }
                set_state(&manager_state, TaskState::Restarting);
                let delay = respawn_wait(streak);
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
    *state.lock() = next;
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
        assert!(matches!(*state.lock(), TaskState::Stopped));
    }

    #[tokio::test(start_paused = true)]
    async fn a_task_that_keeps_failing_is_given_up_after_the_failure_cap() {
        let app = Arc::new(AppState::new(crate::config::Config::default()));
        let mut supervisor = TaskSupervisor::new(app);
        let starts = Arc::new(AtomicUsize::new(0));
        let starts_for_task = Arc::clone(&starts);
        let state = supervisor.register(
            "failing test",
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
        for _ in 0..200 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(starts.load(Ordering::SeqCst), MAX_FAILURES as usize);
        assert!(matches!(*state.lock(), TaskState::Failed(_)));
        supervisor.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_stable_run_forgives_earlier_failures() {
        let app = Arc::new(AppState::new(crate::config::Config::default()));
        let mut supervisor = TaskSupervisor::new(app);
        let starts = Arc::new(AtomicUsize::new(0));
        let starts_for_task = Arc::clone(&starts);
        let state = supervisor.register(
            "occasionally failing test",
            "",
            move || {
                let starts = Arc::clone(&starts_for_task);
                Box::pin(async move {
                    starts.fetch_add(1, Ordering::SeqCst);
                    TaskHandle(tokio::spawn(tokio::time::sleep(STABLE_RUN)))
                })
            },
            || {},
        );
        for _ in 0..600 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }
        assert!(starts.load(Ordering::SeqCst) > MAX_FAILURES as usize);
        assert!(!matches!(*state.lock(), TaskState::Failed(_)));
        supervisor.shutdown().await;
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
        assert!(matches!(*state.lock(), TaskState::Stopped));
    }
}
