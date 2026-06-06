use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::drivers::{
    vendors::generic::devices::common::TaskHandle,
    vendors::nzxt::protocols::NzxtBaseProtocol,
    transports::{hid::HidTransport, Transport},
};
use halod_protocol::types::RgbColor;

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
    Init = 0x60,
    SetFanSpeed = 0x62,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum InitSubCommand {
    SetUpdateInterval = 0x02,
    DetectFans = 0x03,
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
    const REPORT_SIZE: usize = 512;
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
        let channel_byte = 1u8 << channel;

        // Color data packet: [0x26, 0x04, channel_byte, 0x00] + GRB bytes per LED.
        // The hub expects G, R, B order (not RGB).
        let mut pkt = Vec::with_capacity(4 + colors.len() * 3);
        pkt.extend_from_slice(&[0x26, 0x04, channel_byte, 0x00]);
        for c in colors {
            pkt.push(c.g);
            pkt.push(c.r);
            pkt.push(c.b);
        }
        self.base.write(&pkt).await?;

        // Commit packet: [0x26, 0x06, channel_byte, 0x00, 0x01, 0x00, 0x00, 0x18,
        //                  0x00, 0x00, 0x80, 0x00, 0x32, 0x00, 0x00, 0x01, 0x00...]
        let mut apply = [0u8; 64];
        apply[0..16].copy_from_slice(&[
            0x26,
            0x06,
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

    #[allow(dead_code)]
    pub fn parse_fan_config(pkt: &[u8]) -> Option<[u8; FAN_CHANNELS]> {
        if pkt.len() < 16 + FAN_CHANNELS {
            return None;
        }
        if pkt[0] != InputReport::FanConfig as u8 || pkt[1] != 0x03 {
            return None;
        }
        let mut types = [0u8; FAN_CHANNELS];
        for i in 0..FAN_CHANNELS {
            types[i] = pkt[16 + i];
        }
        Some(types)
    }

    #[allow(dead_code)]
    pub async fn handle_packet(&self, pkt: &[u8]) {
        if let Some(updates) = Self::parse_fan_status_speed(pkt) {
            // FAN_STATUS speed report also carries fan_type[i] at offset 16+i.
            let mut cache = self.fan_cache.lock().await;
            for (i, (rpm, duty)) in updates.iter().enumerate() {
                let entry = cache.entry(i as u8).or_default();
                entry.rpm = *rpm;
                entry.duty = *duty;
                if 16 + i < pkt.len() {
                    entry.fan_type = pkt[16 + i];
                }
            }
        } else if let Some(fan_types) = Self::parse_fan_config(pkt) {
            let mut cache = self.fan_cache.lock().await;
            for (i, fan_type) in fan_types.iter().enumerate() {
                cache.entry(i as u8).or_default().fan_type = *fan_type;
            }
        }
    }

    pub async fn detect_fans(&self) {
        self.base
            .write(&[OutputCommand::Init as u8, InitSubCommand::DetectFans as u8][..])
            .await
            .unwrap_or_else(|e| {
                log::error!("[NZXT Control Hub] Failed to detect fans: {}", e);
            });
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
    pub async fn start_polling(&self, interval_ms: u32) {
        // Control byte formula: 0 => 250ms, n => 488 + (n-1)*256 ms
        let control: u8 = if interval_ms <= 250 {
            0
        } else {
            ((interval_ms.saturating_sub(488)) / 256 + 1).min(255) as u8
        };
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

        let transport_clone = self.base.transport.clone();
        let fan_cache = Arc::clone(&self.fan_cache);
        let handle = tokio::task::spawn(async move {
            loop {
                match transport_clone
                    .read_nonblocking(NzxtControlHubProtocol::<HidTransport>::READ_SIZE)
                    .await
                {
                    Ok(pkt) if !pkt.is_empty() => {
                        if let Some(updates) =
                            NzxtControlHubProtocol::<HidTransport>::parse_fan_status_speed(&pkt)
                        {
                            let mut cache = fan_cache.lock().await;
                            for (i, (rpm, duty)) in updates.iter().enumerate() {
                                let entry = cache.entry(i as u8).or_default();
                                entry.rpm = *rpm;
                                entry.duty = *duty;
                                // fan_type lives at offset 16+i in the same
                                // status packet; drives read_fan_controllable().
                                if 16 + i < pkt.len() {
                                    entry.fan_type = pkt[16 + i];
                                }
                            }
                        }
                    }
                    _ => {}
                }
                // Yield between polls so other tasks can acquire the transport
                // lock — without this the read loop can starve writers.
                tokio::time::sleep(POLL_YIELD).await;
            }
        });
        *self.poll_task.lock().await = Some(TaskHandle::new(handle));
    }
}
