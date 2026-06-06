#[cfg(test)]
pub mod test_transport {
    use crate::drivers::transports::Transport;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use tokio::sync::Mutex;

    pub struct MockTransport {
        pub responses: Mutex<VecDeque<Vec<u8>>>,
        pub written: Mutex<Vec<Vec<u8>>>,
    }

    impl MockTransport {
        pub fn new(responses: Vec<Vec<u8>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                written: Mutex::new(Vec::new()),
            }
        }

        pub fn empty() -> Self {
            Self::new(vec![])
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn write(&self, data: &[u8]) -> Result<()> {
            self.written.lock().await.push(data.to_vec());
            Ok(())
        }

        async fn read(&self, _size: usize) -> Result<Vec<u8>> {
            Ok(self.responses.lock().await.pop_front().unwrap_or_default())
        }
    }
}
