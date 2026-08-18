//! Application-wide ownership arbitration for the two MLX service shapes.
//!
//! Studio's video engine and Runtime Hub's chat service use different
//! processes, but they compete for the same large Apple Silicon memory pool.
//! Every transition must therefore serialize the stop/unload step with the
//! start/load step. The guard is intentionally held across active Studio work
//! so a chat request waits for a generation to finish instead of killing its
//! process underneath it.

use std::sync::Arc;

use tokio::sync::{Mutex, OwnedMutexGuard};

#[derive(Clone, Default)]
pub struct MlxOwnershipCoordinator {
    transition: Arc<Mutex<()>>,
}

impl MlxOwnershipCoordinator {
    /// Waits for the sole MLX owner transition slot.
    pub async fn acquire(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.transition).lock_owned().await
    }
}

#[cfg(test)]
mod tests {
    use super::MlxOwnershipCoordinator;
    use std::sync::Arc;
    use tokio::sync::{Barrier, Mutex};

    #[derive(Default)]
    struct ResidentState {
        studio: bool,
        chat: bool,
        violations: usize,
    }

    impl ResidentState {
        fn record(&mut self) {
            if self.studio && self.chat {
                self.violations += 1;
            }
        }
    }

    async fn studio_to_video(
        coordinator: MlxOwnershipCoordinator,
        state: Arc<Mutex<ResidentState>>,
        start: Arc<Barrier>,
    ) {
        start.wait().await;
        let _owner = coordinator.acquire().await;
        {
            let mut state = state.lock().await;
            state.chat = false;
            state.record();
        }
        tokio::task::yield_now().await;
        let mut state = state.lock().await;
        state.studio = true;
        state.record();
    }

    async fn chat_to_mlx(
        coordinator: MlxOwnershipCoordinator,
        state: Arc<Mutex<ResidentState>>,
        start: Arc<Barrier>,
    ) {
        start.wait().await;
        let _owner = coordinator.acquire().await;
        {
            let mut state = state.lock().await;
            state.studio = false;
            state.record();
        }
        tokio::task::yield_now().await;
        let mut state = state.lock().await;
        state.chat = true;
        state.record();
    }

    #[tokio::test]
    async fn concurrent_transitions_never_make_both_processes_resident() {
        let coordinator = MlxOwnershipCoordinator::default();
        let state = Arc::new(Mutex::new(ResidentState {
            studio: true,
            chat: false,
            violations: 0,
        }));
        let start = Arc::new(Barrier::new(3));

        let studio_task = tokio::spawn(studio_to_video(
            coordinator.clone(),
            state.clone(),
            start.clone(),
        ));
        let chat_task = tokio::spawn(chat_to_mlx(coordinator, state.clone(), start.clone()));
        start.wait().await;
        studio_task.await.unwrap();
        chat_task.await.unwrap();

        let state = state.lock().await;
        assert_eq!(state.violations, 0);
        assert_ne!(state.studio, state.chat);
    }

    #[tokio::test]
    async fn a_transition_waits_for_active_studio_work() {
        let coordinator = MlxOwnershipCoordinator::default();
        let active = coordinator.acquire().await;
        let waiting = {
            let coordinator = coordinator.clone();
            tokio::spawn(async move {
                let _owner = coordinator.acquire().await;
                true
            })
        };

        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        drop(active);
        assert!(waiting.await.unwrap());
    }
}
