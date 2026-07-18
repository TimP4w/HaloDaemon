// SPDX-License-Identifier: GPL-3.0-or-later
#[cfg(test)]
pub mod test_transport {
    use crate::drivers::transports::{HidTransport, Transport};
    use crate::drivers::Metered;
    use anyhow::Result;
    use async_trait::async_trait;
    use halod_shared::types::{WriteRateLimit, WriteRateStatus};
    use std::collections::VecDeque;
    use tokio::sync::Mutex;

    pub struct MockTransport {
        pub responses: Mutex<VecDeque<Vec<u8>>>,
        pub written: Mutex<Vec<Vec<u8>>>,
        /// Backs `Transport::rate_status`/`set_write_rate_limit` with the real
        /// gate so tests exercise the actual metering machinery, not a stub.
        rate: Metered<()>,
    }

    impl MockTransport {
        pub fn new(responses: Vec<Vec<u8>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                written: Mutex::new(Vec::new()),
                rate: Metered::new((), None),
            }
        }

        pub fn empty() -> Self {
            Self::new(vec![])
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn write(&self, data: &[u8]) -> Result<()> {
            self.rate.write_access(data.len()).await?;
            self.written.lock().await.push(data.to_vec());
            Ok(())
        }

        async fn read(&self, _size: usize) -> Result<Vec<u8>> {
            self.responses
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("MockTransport: no more responses queued"))
        }

        async fn write_then_read(&self, data: &[u8], _size: usize) -> Result<Vec<u8>> {
            self.rate.write_access(data.len()).await?;
            self.written.lock().await.push(data.to_vec());
            self.responses
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("MockTransport: no more responses queued"))
        }

        fn as_hid(&self) -> Option<&dyn HidTransport> {
            Some(self)
        }

        fn rate_status(&self) -> WriteRateStatus {
            self.rate.status()
        }

        fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>) {
            self.rate.set_limit(limit);
        }
    }

    #[async_trait]
    impl HidTransport for MockTransport {
        async fn feature_exchange(&self, data: &[u8], _response_size: usize) -> Result<Vec<u8>> {
            self.rate.write_access(data.len()).await?;
            self.written.lock().await.push(data.to_vec());
            self.responses
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("MockTransport: no more responses queued"))
        }

        async fn send_feature_report(&self, data: &[u8]) -> Result<()> {
            self.rate.write_access(data.len()).await?;
            self.written.lock().await.push(data.to_vec());
            Ok(())
        }

        async fn get_feature_report(&self, _report_id: u8, _size: usize) -> Result<Vec<u8>> {
            self.responses
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("MockTransport: no more responses queued"))
        }

        async fn get_input_report(&self, _report_id: u8, _size: usize) -> Result<Vec<u8>> {
            self.responses
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("MockTransport: no more responses queued"))
        }

        async fn defer_event(&self, _data: &[u8]) -> Result<()> {
            Ok(())
        }

        async fn write_companion(&self, _data: &[u8]) -> Result<()> {
            anyhow::bail!("MockTransport: no companion collection")
        }

        async fn read_companion(&self, _size: usize) -> Result<Vec<u8>> {
            anyhow::bail!("MockTransport: no companion collection")
        }
    }

    #[tokio::test]
    async fn feature_exchange_records_sent_data_and_returns_response() {
        let transport = MockTransport::new(vec![vec![0xAA, 0xBB]]);
        let response = transport.feature_exchange(&[0x01, 0x02], 2).await.unwrap();
        assert_eq!(response, vec![0xAA, 0xBB]);
        assert_eq!(*transport.written.lock().await, vec![vec![0x01u8, 0x02]]);
    }

    #[tokio::test]
    async fn write_then_read_records_write_and_returns_queued_response() {
        let transport = MockTransport::new(vec![vec![0xCC]]);
        let response = transport.write_then_read(&[0x03, 0x04], 1).await.unwrap();
        assert_eq!(response, vec![0xCC]);
        assert_eq!(*transport.written.lock().await, vec![vec![0x03u8, 0x04]]);
    }

    #[tokio::test]
    async fn write_many_writes_all_packets_in_order() {
        let transport = MockTransport::empty();
        transport
            .write_many(&[vec![0x01], vec![0x02], vec![0x03]])
            .await
            .unwrap();
        let written = transport.written.lock().await;
        assert_eq!(written.len(), 3);
        assert_eq!(written[0], vec![0x01]);
        assert_eq!(written[1], vec![0x02]);
        assert_eq!(written[2], vec![0x03]);
    }
}
