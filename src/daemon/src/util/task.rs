// SPDX-License-Identifier: GPL-3.0-or-later
pub struct TaskHandle(pub tokio::task::JoinHandle<()>);

impl Drop for TaskHandle {
    fn drop(&mut self) {
        self.0.abort();
    }
}
