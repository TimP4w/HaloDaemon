use anyhow::{bail, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::drivers::{
    transports::{hid::HidTransport, Transport},
    vendors::generic::devices::common::TaskHandle,
    vendors::nzxt::protocols::NzxtBaseProtocol,
};
use halod_shared::types::RgbColor;

pub const FAN_CHANNELS: usize = 5;

/// Maximum total LEDs allowed across one channel's chain. The wire protocol
/// accepts arbitrary-length GRB frames, but NZXT firmware caps each Hub
/// channel at 96 LEDs.
pub const MAX_NZXT_CHAIN_LEDS: u32 = 96;

/// Pause between consecutive non-blocking reads in the polling loop, giving
/// other tasks a window to acquire the shared transport lock.
const POLL_YIELD: std::time::Duration = std::time::Duration::from_millis(10);

#[derive(Debug, Clone, Default)]
pub(crate) struct FanChannelState {
    pub rpm: u32,
    pub duty: u8,
    /// Raw fan type from device: 0=none, 1=DC, 2=PWM.
    pub fan_type: u8,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum OutputCommand {
    Lighting = 0x26,
    Init = 0x60,
    SetFanSpeed = 0x62,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum InitSubCommand {
    SetUpdateInterval = 0x02,
    DetectFans = 0x03,
}

/// Sub-command byte for `OutputCommand::Lighting` (`0x26`) packets.
#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum LightingSubCommand {
    Data = 0x04,
    Commit = 0x06,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub(crate) enum InputReport {
    FirmwareVersion = 0x11,
    AccessoryList = 0x21,
    FanConfig = 0x61,
    FanStatus = 0x67,
}

#[derive(Clone)]
pub struct NzxtControlHubProtocol<T: Transport> {
    pub base: NzxtBaseProtocol<T>,
    pub poll_task: Arc<Mutex<Option<TaskHandle>>>,
    pub fan_cache: Arc<Mutex<HashMap<u8, FanChannelState>>>,
}

impl NzxtControlHubProtocol<HidTransport> {
    const REPORT_SIZE: usize = 64;
    const TIMEOUT_MS: i32 = 1000;

    pub fn open(path: &str) -> Result<Self> {
        Ok(Self {
            base: NzxtBaseProtocol::open(path, Self::REPORT_SIZE, Self::TIMEOUT_MS)?,
            poll_task: Arc::new(Mutex::new(None)),
            fan_cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }
}

impl<T: Transport> NzxtControlHubProtocol<T> {
    const READ_SIZE: usize = 64;

    /// Sends a GRB color frame for `channel` followed by a commit packet.
    pub async fn write_rgb_frame(&self, channel: u8, colors: &[RgbColor]) -> Result<()> {
        if channel >= 8 {
            bail!("channel {channel} out of range (max 7) for NZXT hub RGB frame");
        }
        let channel_byte = 1u8 << channel;

        // Color packet: header + GRB bytes per LED (the hub expects G, R, B order, not RGB).
        let mut pkt = Vec::with_capacity(4 + colors.len() * 3);
        pkt.extend_from_slice(&[
            OutputCommand::Lighting as u8,
            LightingSubCommand::Data as u8,
            channel_byte,
            0x00,
        ]);
        for c in colors {
            pkt.extend_from_slice(&[c.g, c.r, c.b]);
        }
        self.base.write(&pkt).await?;

        // Commit packet.
        let mut apply = [0u8; 64];
        apply[0..16].copy_from_slice(&[
            OutputCommand::Lighting as u8,
            LightingSubCommand::Commit as u8,
            channel_byte,
            0x00,
            0x01,
            0x00,
            0x00,
            0x18,
            0x00,
            0x00,
            0x80,
            0x00,
            0x32,
            0x00,
            0x00,
            0x01,
        ]);
        self.base.write(&apply).await?;

        Ok(())
    }

    /// Parses a FAN_STATUS speed report (report_id=0x67, type=0x02).
    /// Layout: rpm[i] at offset 24+i*2 (LE u16), duty[i] at offset 40+i.
    pub fn parse_fan_status_speed(pkt: &[u8]) -> Option<[(u32, u8); FAN_CHANNELS]> {
        if pkt.len() < 40 + FAN_CHANNELS {
            return None;
        }
        if pkt[0] != InputReport::FanStatus as u8 || pkt[1] != 0x02 {
            return None;
        }
        let mut result = [(0u32, 0u8); FAN_CHANNELS];
        for i in 0..FAN_CHANNELS {
            let rpm_offset = 24 + i * 2;
            if rpm_offset + 1 >= pkt.len() {
                return None;
            }
            let rpm = u16::from_le_bytes([pkt[rpm_offset], pkt[rpm_offset + 1]]) as u32;
            let duty = pkt[40 + i];
            result[i] = (rpm, duty);
        }
        Some(result)
    }

    pub fn parse_fan_config(pkt: &[u8]) -> Option<[u8; FAN_CHANNELS]> {
        if pkt.len() < 16 + FAN_CHANNELS {
            return None;
        }
        if pkt[0] != InputReport::FanConfig as u8 || pkt[1] != 0x03 {
            return None;
        }
        let mut types = [0u8; FAN_CHANNELS];
        types.copy_from_slice(&pkt[16..16 + FAN_CHANNELS]);
        Some(types)
    }

    pub async fn handle_packet(&self, pkt: &[u8]) {
        if let Some(updates) = Self::parse_fan_status_speed(pkt) {
            // fan_type[i] lives at offset 16+i in the speed report.
            let mut cache = self.fan_cache.lock().await;
            for (i, (rpm, duty)) in updates.into_iter().enumerate() {
                let entry = cache.entry(i as u8).or_default();
                entry.rpm = rpm;
                entry.duty = duty;
                if 16 + i < pkt.len() {
                    entry.fan_type = pkt[16 + i];
                }
            }
        } else if let Some(fan_types) = Self::parse_fan_config(pkt) {
            let mut cache = self.fan_cache.lock().await;
            for (i, fan_type) in fan_types.into_iter().enumerate() {
                cache.entry(i as u8).or_default().fan_type = fan_type;
            }
        }
    }

    pub async fn detect_fans(&self) -> anyhow::Result<()> {
        self.base
            .write(&[OutputCommand::Init as u8, InitSubCommand::DetectFans as u8][..])
            .await
    }

    pub async fn read_fan_rpm(&self, channel: u8) -> u32 {
        self.fan_cache
            .lock()
            .await
            .get(&channel)
            .map(|s| s.rpm)
            .unwrap_or(0)
    }

    pub async fn read_fan_duty(&self, channel: u8) -> u8 {
        self.fan_cache
            .lock()
            .await
            .get(&channel)
            .map(|s| s.duty)
            .unwrap_or(0)
    }

    pub async fn read_fan_controllable(&self, channel: u8) -> bool {
        self.fan_cache
            .lock()
            .await
            .get(&channel)
            .map(|s| s.fan_type != 0)
            .unwrap_or(false)
    }

    /// Sends SET_FAN_SPEED report (0x62) and optimistically updates local cache.
    /// Report layout: [0x62, 0x01, channel_bitmask, duty[0..8]] (11 bytes total).
    pub async fn write_fan_duty(&self, channel: u8, duty: u8) -> Result<()> {
        if channel as usize >= FAN_CHANNELS {
            anyhow::bail!(
                "Fan channel {} out of range (max {})",
                channel,
                FAN_CHANNELS - 1
            );
        }
        let mut pkt = [0u8; 11];
        pkt[0] = OutputCommand::SetFanSpeed as u8;
        pkt[1] = 0x01;
        pkt[2] = 1u8 << channel;
        pkt[3 + channel as usize] = duty;
        self.base.write(&pkt).await?;
        self.fan_cache.lock().await.entry(channel).or_default().duty = duty;
        Ok(())
    }
}

impl NzxtControlHubProtocol<HidTransport> {
    /// Convert an interval in milliseconds to the firmware control byte.
    ///
    /// Formula: 0 => 250ms, n => 488 + (n-1)*256 ms
    pub(super) fn interval_control_byte(interval_ms: u32) -> u8 {
        if interval_ms <= 250 {
            0
        } else {
            // Intervals in (250, 488) ms are rounded up to 488 ms — the protocol
            // minimum non-zero interval (n=1 → 488 + 0×256 ms).
            ((interval_ms.saturating_sub(488)) / 256 + 1).min(255) as u8
        }
    }

    pub async fn start_polling(&self, interval_ms: u32) {
        let control = Self::interval_control_byte(interval_ms);
        self.base
            .write(
                &[
                    OutputCommand::Init as u8,
                    InitSubCommand::SetUpdateInterval as u8,
                    0x01,
                    0xE8,
                    control,
                    0x01,
                    0xE8,
                    control,
                ][..],
            )
            .await
            .unwrap_or_else(|e| {
                log::error!("[NZXT Control Hub] Failed to set update interval: {}", e);
            });

        let this = self.clone();
        let handle = tokio::task::spawn(async move {
            loop {
                match this
                    .base
                    .transport
                    .read_nonblocking(NzxtControlHubProtocol::<HidTransport>::READ_SIZE)
                    .await
                {
                    Ok(pkt) if !pkt.is_empty() => {
                        this.handle_packet(&pkt).await;
                    }
                    Ok(_) => {} // empty read — normal when no new status pushed
                    Err(e) => {
                        log::trace!("[NZXT Control Hub] poll read error: {e:#}");
                    }
                }
                tokio::time::sleep(POLL_YIELD).await;
            }
        });
        *self.poll_task.lock().await = Some(TaskHandle::new(handle));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::transports::mock::test_transport::MockTransport;

    fn protocol(responses: Vec<Vec<u8>>) -> NzxtControlHubProtocol<MockTransport> {
        NzxtControlHubProtocol {
            base: NzxtBaseProtocol::new(MockTransport::new(responses)),
            poll_task: Arc::new(Mutex::new(None)),
            fan_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    // ── parse_fan_status_speed ────────────────────────────────────────────

    #[test]
    fn parse_fan_status_speed_returns_correct_rpm_and_duty() {
        let mut pkt = vec![0u8; 45];
        pkt[0] = 0x67; // InputReport::FanStatus
        pkt[1] = 0x02; // speed subtype
                       // RPM (LE u16) at offsets 24, 26, 28, 30, 32
        pkt[24] = 0xD2;
        pkt[25] = 0x04; // ch0: 1234
        pkt[26] = 0xC4;
        pkt[27] = 0x09; // ch1: 2500
        pkt[28] = 0x00;
        pkt[29] = 0x00; // ch2: 0
        pkt[30] = 0x8A;
        pkt[31] = 0x02; // ch3: 650
        pkt[32] = 0x08;
        pkt[33] = 0x07; // ch4: 1800
                        // Duty at offsets 40..44
        pkt[40] = 50;
        pkt[41] = 100;
        pkt[42] = 0;
        pkt[43] = 30;
        pkt[44] = 75;

        let result = NzxtControlHubProtocol::<MockTransport>::parse_fan_status_speed(&pkt);
        assert!(result.is_some());
        let channels = result.unwrap();
        assert_eq!(channels[0], (1234, 50));
        assert_eq!(channels[1], (2500, 100));
        assert_eq!(channels[2], (0, 0));
        assert_eq!(channels[3], (650, 30));
        assert_eq!(channels[4], (1800, 75));
    }

    #[test]
    fn parse_fan_status_speed_rejects_short_packet() {
        assert!(
            NzxtControlHubProtocol::<MockTransport>::parse_fan_status_speed(&[0u8; 44]).is_none()
        );
        assert!(
            NzxtControlHubProtocol::<MockTransport>::parse_fan_status_speed(&[0u8; 4]).is_none()
        );
    }

    #[test]
    fn parse_fan_status_speed_rejects_wrong_prefix() {
        let mut pkt = vec![0u8; 45];
        pkt[0] = 0x67;
        pkt[1] = 0x01; // wrong subtype
        assert!(NzxtControlHubProtocol::<MockTransport>::parse_fan_status_speed(&pkt).is_none());
        let mut pkt2 = vec![0u8; 45];
        pkt2[0] = 0x75;
        pkt2[1] = 0x02; // wrong report id
        assert!(NzxtControlHubProtocol::<MockTransport>::parse_fan_status_speed(&pkt2).is_none());
    }

    // ── parse_fan_config ──────────────────────────────────────────────────

    #[test]
    fn parse_fan_config_returns_correct_fan_types() {
        let mut pkt = vec![0u8; 21];
        pkt[0] = 0x61; // InputReport::FanConfig
        pkt[1] = 0x03;
        pkt[16] = 2;
        pkt[17] = 2;
        pkt[18] = 1;
        pkt[19] = 0;
        pkt[20] = 2;
        let result = NzxtControlHubProtocol::<MockTransport>::parse_fan_config(&pkt);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), [2, 2, 1, 0, 2]);
    }

    #[test]
    fn parse_fan_config_rejects_short_packet() {
        assert!(NzxtControlHubProtocol::<MockTransport>::parse_fan_config(&[0u8; 20]).is_none());
    }

    #[test]
    fn parse_fan_config_rejects_wrong_prefix() {
        let mut pkt = vec![0u8; 21];
        pkt[0] = 0x61;
        pkt[1] = 0x01;
        assert!(NzxtControlHubProtocol::<MockTransport>::parse_fan_config(&pkt).is_none());
        let mut pkt2 = vec![0u8; 21];
        pkt2[0] = 0x67;
        pkt2[1] = 0x03;
        assert!(NzxtControlHubProtocol::<MockTransport>::parse_fan_config(&pkt2).is_none());
    }

    // ── write_fan_duty ────────────────────────────────────────────────────

    #[tokio::test]
    async fn write_fan_duty_sends_correct_11_byte_packet() {
        let p = protocol(vec![]);
        p.write_fan_duty(0, 50).await.unwrap();
        let written = p.base.transport.written.lock().await;
        assert_eq!(written.len(), 1);
        assert_eq!(written[0], [0x62, 0x01, 0x01, 50, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[tokio::test]
    async fn write_fan_duty_sets_correct_bitmask_for_channel_2() {
        let p = protocol(vec![]);
        p.write_fan_duty(2, 75).await.unwrap();
        let written = p.base.transport.written.lock().await;
        assert_eq!(written[0], [0x62, 0x01, 0x04, 0, 0, 75, 0, 0, 0, 0, 0]);
    }

    #[tokio::test]
    async fn write_fan_duty_rejects_channel_out_of_range() {
        let p = protocol(vec![]);
        assert!(p.write_fan_duty(5, 50).await.is_err());
        assert!(p.base.transport.written.lock().await.is_empty());
    }

    #[tokio::test]
    async fn write_fan_duty_updates_cache() {
        let p = protocol(vec![]);
        p.write_fan_duty(1, 60).await.unwrap();
        assert_eq!(p.read_fan_duty(1).await, 60);
    }

    // ── interval_control_byte ─────────────────────────────────────────────

    #[test]
    fn interval_control_byte_250ms_or_less_is_zero() {
        assert_eq!(
            NzxtControlHubProtocol::<HidTransport>::interval_control_byte(0),
            0
        );
        assert_eq!(
            NzxtControlHubProtocol::<HidTransport>::interval_control_byte(250),
            0
        );
    }

    #[test]
    fn interval_control_byte_488ms_is_1() {
        assert_eq!(
            NzxtControlHubProtocol::<HidTransport>::interval_control_byte(488),
            1
        );
    }

    #[test]
    fn interval_control_byte_744ms_is_2() {
        assert_eq!(
            NzxtControlHubProtocol::<HidTransport>::interval_control_byte(744),
            2
        );
    }

    #[test]
    fn interval_control_byte_large_value_saturates() {
        assert_eq!(
            NzxtControlHubProtocol::<HidTransport>::interval_control_byte(u32::MAX),
            255
        );
    }

    #[test]
    fn interval_control_byte_251ms_produces_expected_value() {
        // Just above 250: 251 - 488 = negative → saturating_sub = 0 → 0/256+1 = 1
        assert_eq!(
            NzxtControlHubProtocol::<HidTransport>::interval_control_byte(251),
            1
        );
    }
}
