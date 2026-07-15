// SPDX-License-Identifier: GPL-3.0-or-later
//! Endpoint-oriented, scoped USB transport used by plugins.

use std::{collections::HashMap, sync::Mutex, time::Duration};

use anyhow::{anyhow, bail, Context as _, Result};
use halod_shared::types::{WriteRateLimit, WriteRateStatus};
use rusb::{Context, Device, Direction, TransferType, UsbContext};

use crate::drivers::plugins::manifest::{
    UsbControlConfig, UsbDeviceConfig, UsbEndpointConfig, UsbTransferType,
};
use crate::drivers::Metered;

use super::UsbClaim;

#[derive(Debug, Clone, Default)]
pub struct UsbSelector {
    pub vid: u16,
    pub pid: u16,
    pub bus: Option<u8>,
    pub address: Option<u8>,
    pub port_path: Vec<u8>,
    pub serial: Option<String>,
    pub index: usize,
}

#[derive(Clone)]
struct EndpointPolicy {
    transfer_type: UsbTransferType,
    max_transfer_size: usize,
    max_timeout_ms: u64,
}

pub struct UsbEndpointTransport {
    io: Metered<UsbClaim>,
    endpoints: HashMap<u8, EndpointPolicy>,
    control: Option<UsbControlConfig>,
    locator: UsbSelector,
    transfer_lock: Mutex<()>,
}

impl UsbEndpointTransport {
    pub fn open(
        selector: &UsbSelector,
        config: &UsbDeviceConfig,
        affinity: Option<&[u8]>,
        limit: Option<WriteRateLimit>,
    ) -> Result<Self> {
        let ctx = Context::new()?;
        let (device, descriptor) = find_device(&ctx, selector, affinity)?;
        let handle = device.open().context("opening USB device")?;
        let active = device.active_config_descriptor()?;
        let interface = config.interface.unwrap_or_else(|| {
            config
                .endpoints
                .iter()
                .find_map(|wanted| endpoint_interface(&active, wanted))
                .unwrap_or(0)
        });
        let claim = UsbClaim::claim(handle, interface)
            .with_context(|| format!("claiming USB interface {interface}"))?;
        if let Some(alternate) = config.alternate_setting {
            claim
                .handle
                .set_alternate_setting(interface, alternate)
                .with_context(|| {
                    format!("selecting USB interface {interface} alternate {alternate}")
                })?;
        }
        for wanted in &config.endpoints {
            validate_endpoint_descriptor(&active, interface, config.alternate_setting, wanted)?;
        }
        let locator = UsbSelector {
            vid: descriptor.vendor_id(),
            pid: descriptor.product_id(),
            bus: Some(device.bus_number()),
            address: Some(device.address()),
            port_path: device.port_numbers().unwrap_or_default(),
            serial: selector.serial.clone(),
            index: selector.index,
        };
        Ok(Self {
            io: Metered::new(claim, limit),
            endpoints: config
                .endpoints
                .iter()
                .map(|ep| {
                    (
                        ep.address,
                        EndpointPolicy {
                            transfer_type: ep.transfer_type,
                            max_transfer_size: ep.max_transfer_size,
                            max_timeout_ms: ep.max_timeout_ms,
                        },
                    )
                })
                .collect(),
            control: config.control.clone(),
            locator,
            transfer_lock: Mutex::new(()),
        })
    }

    pub fn locator(&self) -> &UsbSelector {
        &self.locator
    }

    fn endpoint(
        &self,
        endpoint: u8,
        length: usize,
        timeout_ms: u64,
        direction: Direction,
    ) -> Result<&EndpointPolicy> {
        let policy = self.endpoints.get(&endpoint).ok_or_else(|| {
            anyhow!("USB endpoint 0x{endpoint:02x} is outside the manifest allowlist")
        })?;
        let endpoint_direction = if endpoint & 0x80 == 0 {
            Direction::Out
        } else {
            Direction::In
        };
        if endpoint_direction != direction {
            bail!("USB endpoint 0x{endpoint:02x} has the wrong direction");
        }
        if length > policy.max_transfer_size {
            bail!(
                "USB transfer length {length} exceeds endpoint 0x{endpoint:02x} bound {}",
                policy.max_transfer_size
            );
        }
        if timeout_ms == 0 || timeout_ms > policy.max_timeout_ms {
            bail!(
                "USB timeout {timeout_ms}ms exceeds endpoint 0x{endpoint:02x} bound {}ms",
                policy.max_timeout_ms
            );
        }
        Ok(policy)
    }

    pub fn write(&self, endpoint: u8, data: &[u8], timeout_ms: u64) -> Result<usize> {
        let policy = self
            .endpoint(endpoint, data.len(), timeout_ms, Direction::Out)?
            .clone();
        let _guard = self
            .transfer_lock
            .lock()
            .map_err(|_| anyhow!("USB transfer mutex poisoned"))?;
        let claim = self.io.write_access_blocking(data.len())?;
        let timeout = Duration::from_millis(timeout_ms);
        let mut sent = 0;
        while sent < data.len() {
            let n = match policy.transfer_type {
                UsbTransferType::Bulk => claim.handle.write_bulk(endpoint, &data[sent..], timeout),
                UsbTransferType::Interrupt => {
                    claim
                        .handle
                        .write_interrupt(endpoint, &data[sent..], timeout)
                }
            }
            .context("USB endpoint write failed")?;
            if n == 0 {
                bail!(
                    "USB endpoint 0x{endpoint:02x} stalled after {sent}/{} bytes",
                    data.len()
                );
            }
            sent += n;
        }
        Ok(sent)
    }

    pub fn read(&self, endpoint: u8, length: usize, timeout_ms: u64) -> Result<Vec<u8>> {
        let policy = self
            .endpoint(endpoint, length, timeout_ms, Direction::In)?
            .clone();
        let _guard = self
            .transfer_lock
            .lock()
            .map_err(|_| anyhow!("USB transfer mutex poisoned"))?;
        let mut data = vec![0; length];
        let claim = self.io.read_access();
        let timeout = Duration::from_millis(timeout_ms);
        let n = match policy.transfer_type {
            UsbTransferType::Bulk => claim.handle.read_bulk(endpoint, &mut data, timeout),
            UsbTransferType::Interrupt => claim.handle.read_interrupt(endpoint, &mut data, timeout),
        }
        .context("USB endpoint read failed")?;
        data.truncate(n.min(data.len()));
        Ok(data)
    }

    pub fn control(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        bytes: &[u8],
        read_length: usize,
        timeout_ms: u64,
    ) -> Result<UsbControlResult> {
        let policy = self
            .control
            .as_ref()
            .ok_or_else(|| anyhow!("USB control transfers are not declared for this device"))?;
        let length = bytes.len().max(read_length);
        if length > policy.max_transfer_size {
            bail!(
                "USB control transfer length {length} exceeds bound {}",
                policy.max_transfer_size
            );
        }
        if timeout_ms == 0 || timeout_ms > policy.max_timeout_ms {
            bail!(
                "USB control timeout {timeout_ms}ms exceeds bound {}ms",
                policy.max_timeout_ms
            );
        }
        let input = request_type & 0x80 != 0;
        if input && !bytes.is_empty() {
            bail!("USB control IN transfer cannot include output bytes");
        }
        if !input && read_length != 0 {
            bail!("USB control OUT transfer cannot request input bytes");
        }
        let _guard = self
            .transfer_lock
            .lock()
            .map_err(|_| anyhow!("USB transfer mutex poisoned"))?;
        let timeout = Duration::from_millis(timeout_ms);
        if input {
            let mut data = vec![0; read_length];
            let n = self
                .io
                .read_access()
                .handle
                .read_control(request_type, request, value, index, &mut data, timeout)
                .context("USB control read failed")?;
            data.truncate(n.min(data.len()));
            Ok(UsbControlResult::Read(data))
        } else {
            let claim = self.io.write_access_blocking(bytes.len())?;
            let n = claim
                .handle
                .write_control(request_type, request, value, index, bytes, timeout)
                .context("USB control write failed")?;
            Ok(UsbControlResult::Written(n))
        }
    }

    pub fn rate_status(&self) -> WriteRateStatus {
        self.io.status()
    }
}

pub enum UsbControlResult {
    Written(usize),
    Read(Vec<u8>),
}

pub trait UsbCollection: Send + Sync {
    fn write(
        &self,
        device_id: Option<&str>,
        endpoint: u8,
        data: &[u8],
        timeout_ms: u64,
    ) -> Result<usize>;
    fn read(
        &self,
        device_id: Option<&str>,
        endpoint: u8,
        length: usize,
        timeout_ms: u64,
    ) -> Result<Vec<u8>>;
    #[allow(clippy::too_many_arguments)]
    fn control(
        &self,
        device_id: Option<&str>,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        bytes: &[u8],
        read_length: usize,
        timeout_ms: u64,
    ) -> Result<UsbControlResult>;
    fn rate_status(&self) -> WriteRateStatus;
}

pub struct UsbDevices {
    devices: HashMap<String, UsbEndpointTransport>,
}

impl UsbDevices {
    pub fn open(
        selector: UsbSelector,
        configs: &[UsbDeviceConfig],
        limit: Option<WriteRateLimit>,
    ) -> Result<Self> {
        let primary_cfg = configs
            .iter()
            .find(|d| d.id == "primary")
            .ok_or_else(|| anyhow!("USB transport has no primary device"))?;
        let primary = UsbEndpointTransport::open(&selector, primary_cfg, None, limit)?;
        let affinity = primary.locator().port_path.clone();
        let mut devices = HashMap::from([("primary".to_owned(), primary)]);
        for cfg in configs.iter().filter(|d| d.id != "primary") {
            let companion = UsbSelector {
                vid: cfg.vid.expect("validated companion vid"),
                pid: cfg.pid.expect("validated companion pid"),
                ..Default::default()
            };
            devices.insert(
                cfg.id.clone(),
                UsbEndpointTransport::open(&companion, cfg, Some(&affinity), limit)?,
            );
        }
        Ok(Self { devices })
    }
    pub fn get(&self, id: Option<&str>) -> Result<&UsbEndpointTransport> {
        let id = id.unwrap_or("primary");
        self.devices
            .get(id)
            .ok_or_else(|| anyhow!("unknown USB device '{id}'"))
    }
    pub fn rate_status(&self) -> WriteRateStatus {
        self.devices
            .get("primary")
            .map(UsbEndpointTransport::rate_status)
            .unwrap_or_default()
    }
}

impl UsbCollection for UsbDevices {
    fn write(
        &self,
        device_id: Option<&str>,
        endpoint: u8,
        data: &[u8],
        timeout_ms: u64,
    ) -> Result<usize> {
        self.get(device_id)?.write(endpoint, data, timeout_ms)
    }
    fn read(
        &self,
        device_id: Option<&str>,
        endpoint: u8,
        length: usize,
        timeout_ms: u64,
    ) -> Result<Vec<u8>> {
        self.get(device_id)?.read(endpoint, length, timeout_ms)
    }
    fn control(
        &self,
        device_id: Option<&str>,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        bytes: &[u8],
        read_length: usize,
        timeout_ms: u64,
    ) -> Result<UsbControlResult> {
        self.get(device_id)?.control(
            request_type,
            request,
            value,
            index,
            bytes,
            read_length,
            timeout_ms,
        )
    }
    fn rate_status(&self) -> WriteRateStatus {
        UsbDevices::rate_status(self)
    }
}

fn find_device(
    ctx: &Context,
    selector: &UsbSelector,
    affinity: Option<&[u8]>,
) -> Result<(Device<Context>, rusb::DeviceDescriptor)> {
    let mut found = Vec::new();
    for device in ctx.devices()?.iter() {
        let Ok(desc) = device.device_descriptor() else {
            continue;
        };
        if desc.vendor_id() != selector.vid || desc.product_id() != selector.pid {
            continue;
        }
        if selector.bus.is_some_and(|v| v != device.bus_number())
            || selector.address.is_some_and(|v| v != device.address())
        {
            continue;
        }
        if !selector.port_path.is_empty()
            && device.port_numbers().ok().as_deref() != Some(selector.port_path.as_slice())
        {
            continue;
        }
        if let Some(serial) = selector.serial.as_deref() {
            let matches = device
                .open()
                .ok()
                .and_then(|h| h.read_serial_number_string_ascii(&desc).ok())
                .is_some_and(|s| s == serial);
            if !matches {
                continue;
            }
        }
        found.push((device, desc));
    }
    found.sort_by_key(|(d, _)| {
        let ports = d.port_numbers().unwrap_or_default();
        let common = affinity
            .map(|a| ports.iter().zip(a).take_while(|(x, y)| x == y).count())
            .unwrap_or(0);
        (
            std::cmp::Reverse(common),
            d.bus_number(),
            ports,
            d.address(),
        )
    });
    found.into_iter().nth(selector.index).ok_or_else(|| {
        anyhow!(
            "USB device {:04x}:{:04x} not found",
            selector.vid,
            selector.pid
        )
    })
}

fn endpoint_interface(config: &rusb::ConfigDescriptor, wanted: &UsbEndpointConfig) -> Option<u8> {
    config
        .interfaces()
        .flat_map(|i| i.descriptors())
        .find(|d| {
            d.endpoint_descriptors()
                .any(|e| e.address() == wanted.address)
        })
        .map(|d| d.interface_number())
}

fn validate_endpoint_descriptor(
    config: &rusb::ConfigDescriptor,
    interface: u8,
    alternate: Option<u8>,
    wanted: &UsbEndpointConfig,
) -> Result<()> {
    let expected = match wanted.transfer_type {
        UsbTransferType::Bulk => TransferType::Bulk,
        UsbTransferType::Interrupt => TransferType::Interrupt,
    };
    let found = config
        .interfaces()
        .flat_map(|i| i.descriptors())
        .filter(|d| {
            d.interface_number() == interface && alternate.is_none_or(|a| d.setting_number() == a)
        })
        .flat_map(|d| d.endpoint_descriptors())
        .any(|e| e.address() == wanted.address && e.transfer_type() == expected);
    if !found {
        bail!(
            "declared {:?} endpoint 0x{:02x} was not found on interface {interface}",
            wanted.transfer_type,
            wanted.address
        );
    }
    Ok(())
}
