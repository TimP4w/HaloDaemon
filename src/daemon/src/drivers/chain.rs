// SPDX-License-Identifier: GPL-3.0-or-later
//! Vendor-agnostic chain machinery.
//!
//! Parent drivers that expose chainable ARGB channels embed a [`ChainHost`]
//! and implement [`ChainAdapter`]. The host owns all chain state and CRUD;
//! the adapter is the small vendor-specific surface that just enumerates
//! channels and writes composed frames to the wire.
//!
//! Children — instances of
//! [`crate::drivers::vendors::generic::devices::generic_argb::GenericArgb`] —
//! talk to the host through the [`ChainHub`] trait.
//!
//! See `docs/chainable-argb.md` for the end-to-end walkthrough.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Mutex as TokioMutex;

use halod_shared::types::{ChainLinkInfo, ChainableChannelInfo, RgbColor, ZoneTopology};

use crate::drivers::vendors::generic::devices::generic_argb::GenericArgb;
use crate::drivers::{ChainLinkSpec, Device};
use crate::registry::config::ChainLinkRecord;

#[derive(Debug, Clone)]
pub struct ChainLinkRuntime {
    pub child_id: String,
    pub name: String,
    pub topology: ZoneTopology,
    pub led_count: u32,
    /// True for hardware-detected accessories; locked links reject user mutation.
    pub locked: bool,
    pub last_colors: Vec<RgbColor>,
}

impl ChainLinkRuntime {
    pub fn new(
        child_id: String,
        name: String,
        topology: ZoneTopology,
        led_count: u32,
        locked: bool,
    ) -> Self {
        Self {
            child_id,
            name,
            topology,
            led_count,
            locked,
            last_colors: Vec::new(),
        }
    }

    pub fn info(&self) -> ChainLinkInfo {
        ChainLinkInfo {
            child_device_id: self.child_id.clone(),
            name: self.name.clone(),
            topology: self.topology.clone(),
            led_count: self.led_count,
            locked: self.locked,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ChannelChainState {
    pub links: Vec<ChainLinkRuntime>,
    pub discovery_active: bool,
}

impl ChannelChainState {
    pub fn used_leds(&self) -> u32 {
        self.links.iter().map(|l| l.led_count).sum()
    }

    pub fn find_index(&self, child_id: &str) -> Option<usize> {
        self.links.iter().position(|l| l.child_id == child_id)
    }

    pub fn update_slot(&mut self, child_id: &str, colors: &[RgbColor]) -> bool {
        match self.find_index(child_id) {
            Some(i) => {
                self.links[i].last_colors = colors.to_vec();
                true
            }
            None => false,
        }
    }

    pub fn composed_frame(&self) -> Vec<RgbColor> {
        let total: usize = self.links.iter().map(|l| l.led_count as usize).sum();
        let mut out = Vec::with_capacity(total);
        let black = RgbColor { r: 0, g: 0, b: 0 };
        for link in &self.links {
            for i in 0..link.led_count as usize {
                out.push(link.last_colors.get(i).copied().unwrap_or(black));
            }
        }
        out
    }

    pub fn append(&mut self, link: ChainLinkRuntime) {
        self.links.push(link);
    }

    pub fn remove(&mut self, child_id: &str) -> Result<(), &'static str> {
        let Some(i) = self.find_index(child_id) else {
            return Err("chain link not found");
        };
        if self.links[i].locked {
            return Err("chain link is locked");
        }
        self.links.remove(i);
        Ok(())
    }

    pub fn rename(&mut self, child_id: &str, new_name: &str) -> Result<(), &'static str> {
        let Some(i) = self.find_index(child_id) else {
            return Err("chain link not found");
        };
        if self.links[i].locked {
            return Err("chain link is locked");
        }
        self.links[i].name = new_name.to_string();
        Ok(())
    }

    pub fn reorder(&mut self, child_id: &str, new_index: usize) -> Result<(), &'static str> {
        let Some(i) = self.find_index(child_id) else {
            return Err("chain link not found");
        };
        if self.links[i].locked {
            return Err("chain link is locked");
        }
        let link = self.links.remove(i);
        let target = new_index.min(self.links.len());
        // Position 0 is reserved for the hardware-detected (locked) accessory.
        let target = if self.links.first().map(|l| l.locked).unwrap_or(false) && target == 0 {
            1
        } else {
            target
        };
        self.links.insert(target, link);
        Ok(())
    }
}

/// One chainable channel exposed by a parent. Vendor-specific addressing is
/// hidden behind `channel_id`; the rest of the system uses strings everywhere.
#[derive(Debug, Clone)]
pub struct ChannelDescriptor {
    pub channel_id: String,
    pub display_name: String,
    pub max_leds: u32,
}

/// Vendor-specific surface a parent must implement. Tiny on purpose — the
/// shared CRUD lives in [`ChainHost`].
#[async_trait]
pub trait ChainAdapter: Send + Sync + 'static {
    fn parent_id(&self) -> String;

    /// Enumerate every chainable channel this parent exposes, in stable order.
    fn channels(&self) -> Vec<ChannelDescriptor>;

    /// Write `composed` (already a single contiguous color array for the whole
    /// channel) to the wire.
    async fn write_composed_frame(&self, channel_id: &str, composed: &[RgbColor]) -> Result<()>;
}

/// Child-side interface. Generic ARGB children call this to push their slice
/// of the chain frame.
#[async_trait]
pub trait ChainHub: Send + Sync + 'static {
    async fn write_chain_slice(
        &self,
        channel_id: &str,
        child_device_id: &str,
        colors: &[RgbColor],
    ) -> Result<()>;

    /// Look up the runtime name of a chain link. Children call this from
    /// `serialize()` so a `set_device_name` IPC (routed via
    /// `ChainCapability::rename_chain_link` for external-name devices) reaches
    /// the wire device label without per-child name storage.
    fn link_name(&self, channel_id: &str, child_device_id: &str) -> Option<String>;
}

/// Shared chain runtime. Owns state + adapter + the Arc handles to spawned
/// children; drivers embed an `Arc<ChainHost>` and forward their
/// [`crate::drivers::ChainCapability`] impls here.
pub struct ChainHost {
    adapter: Arc<dyn ChainAdapter>,
    /// `std::sync::Mutex`: operations never cross `.await` while holding it.
    pub(crate) state: Mutex<HashMap<String, ChannelChainState>>,
    /// Every device the host knows about; `TokioMutex` since `serialize()` reads it async.
    children: TokioMutex<Vec<Arc<dyn Device>>>,
}

impl ChainHost {
    /// Build a host wrapping `adapter`, pre-seeded with an empty chain per channel.
    pub fn new(adapter: Arc<dyn ChainAdapter>) -> Arc<Self> {
        let mut state: HashMap<String, ChannelChainState> = HashMap::new();
        for d in adapter.channels() {
            state.insert(d.channel_id, ChannelChainState::default());
        }
        Arc::new(Self {
            adapter,
            state: Mutex::new(state),
            children: TokioMutex::new(Vec::new()),
        })
    }

    /// Snapshot of every Arc the host currently tracks. Parent drivers call
    /// this from their `serialize()` to populate
    /// [`DeviceCapability::Children`].
    pub async fn children(&self) -> Vec<Arc<dyn Device>> {
        self.children.lock().await.clone()
    }

    /// Snapshot of every chainable channel + its current links. Sync method
    /// called from the device serializer.
    pub fn chainable_channels(&self) -> Vec<ChainableChannelInfo> {
        let channels = self.adapter.channels();
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        channels
            .into_iter()
            .map(|d| {
                let empty = ChannelChainState::default();
                let chain = state.get(&d.channel_id).unwrap_or(&empty);
                channel_info(&d.channel_id, &d.display_name, d.max_leds, chain)
            })
            .collect()
    }

    /// Register a hardware-detected first link (locked). Called from the
    /// parent's `discover_children` after `child.initialize()` succeeds. The
    /// host tracks the child Arc itself so the parent's `serialize()` reads
    /// children directly from [`ChainHost::children`].
    pub async fn register_auto_link(&self, channel_id: &str, child: Arc<dyn Device>) {
        let Some(rgb) = child.as_rgb() else {
            return;
        };
        let desc = rgb.descriptor().clone();
        let Some(zone) = desc.zones.first() else {
            return;
        };
        let link = ChainLinkRuntime::new(
            child.id().to_owned(),
            child.name().to_string(),
            zone.topology.clone(),
            u32::try_from(zone.leds.len()).unwrap_or_else(|_| {
                log::warn!(
                    "[chain] LED count {} truncated to u32::MAX",
                    zone.leds.len()
                );
                u32::MAX
            }),
            /* locked */ true,
        );
        let displaced = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let chain = state.entry(channel_id.to_string()).or_default();
            if chain.links.first().map(|l| l.locked).unwrap_or(false) {
                let old = chain.links[0].child_id.clone();
                chain.links[0] = link;
                Some(old)
            } else {
                chain.links.insert(0, link);
                None
            }
        };
        // Drop any Arc displaced by a replace so `children()` stays in sync with link state.
        let mut children = self.children.lock().await;
        let cid = child.id().to_owned();
        children.retain(|c| c.id() != cid && Some(c.id()) != displaced.as_deref());
        children.push(child);
    }

    fn max_leds(&self, channel_id: &str) -> Option<u32> {
        self.adapter
            .channels()
            .into_iter()
            .find(|d| d.channel_id == channel_id)
            .map(|d| d.max_leds)
    }

    /// Append a new user-added chain link, spawn its child device, register it
    /// in `app.devices` and return the new child's id. Validation:
    /// - the channel must exist
    /// - `led_count` must fit within remaining budget
    /// - `led_count` must satisfy the topology's divisibility constraint
    ///   (Rings)
    pub async fn add_link(
        self: &Arc<Self>,
        channel_id: &str,
        spec: ChainLinkSpec,
    ) -> Result<(String, Arc<dyn Device>)> {
        let new_id = format!(
            "{}_chain_{channel_id}_{}",
            self.adapter.parent_id(),
            uuid::Uuid::new_v4()
        );
        let device = self
            .spawn_link(
                channel_id,
                &new_id,
                spec.name,
                spec.topology,
                spec.led_count,
            )
            .await?;
        Ok((new_id, device))
    }

    /// Shared spawn→reserve→register pipeline for `add_link` and `restore_link`.
    /// Inits child before reserving slot so a failure cannot orphan a link entry.
    async fn spawn_link(
        self: &Arc<Self>,
        channel_id: &str,
        child_id: &str,
        name: String,
        topology: ZoneTopology,
        led_count: u32,
    ) -> Result<Arc<dyn Device>> {
        validate_led_count(&topology, led_count).map_err(|e| anyhow::anyhow!("{e}"))?;
        let max_leds = self
            .max_leds(channel_id)
            .ok_or_else(|| anyhow::anyhow!("unknown chainable channel: {channel_id}"))?;

        // Spawn and initialize *before* reserving the slot under the lock, so
        // a failed init cannot orphan a link entry or race with remove_link.
        let child = self.spawn_child(
            channel_id,
            child_id,
            name.clone(),
            topology.clone(),
            led_count,
        );
        child.initialize().await?;

        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let chain = state.entry(channel_id.to_string()).or_default();
            let used = chain.used_leds();
            if used.saturating_add(led_count) > max_leds {
                anyhow::bail!(
                    "LED budget exceeded on channel {channel_id}: {used} + {led_count} > {max_leds}"
                );
            }
            chain.append(ChainLinkRuntime::new(
                child_id.to_string(),
                name,
                topology,
                led_count,
                false,
            ));
        }

        let device: Arc<dyn Device> = child;
        self.children.lock().await.push(device.clone());
        Ok(device)
    }

    pub async fn remove_link(&self, channel_id: &str, child_id: &str) -> Result<String> {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let chain = state
                .get_mut(channel_id)
                .ok_or_else(|| anyhow::anyhow!("unknown channel: {channel_id}"))?;
            chain.remove(child_id).map_err(anyhow::Error::msg)?;
        }
        self.children.lock().await.retain(|d| d.id() != child_id);
        Ok(child_id.to_string())
    }

    pub fn rename_link(&self, channel_id: &str, child_id: &str, new_name: &str) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let chain = state
            .get_mut(channel_id)
            .ok_or_else(|| anyhow::anyhow!("unknown channel: {channel_id}"))?;
        chain
            .rename(child_id, new_name)
            .map_err(anyhow::Error::msg)?;
        Ok(())
    }

    pub fn reorder_link(&self, channel_id: &str, child_id: &str, new_index: usize) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let chain = state
            .get_mut(channel_id)
            .ok_or_else(|| anyhow::anyhow!("unknown channel: {channel_id}"))?;
        chain
            .reorder(child_id, new_index)
            .map_err(anyhow::Error::msg)?;
        Ok(())
    }

    /// Flash `max_leds` red LEDs on `channel_id` three times to help the user
    /// identify which physical port a device is attached to.  Suppresses canvas
    /// engine writes for the channel during the animation via `discovery_active`.
    pub async fn detect_channel(&self, channel_id: &str) -> Result<()> {
        let max_leds = self
            .max_leds(channel_id)
            .ok_or_else(|| anyhow::anyhow!("unknown channel: {channel_id}"))?;

        let saved = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let chain = state
                .get_mut(channel_id)
                .ok_or_else(|| anyhow::anyhow!("unknown channel: {channel_id}"))?;
            let saved = chain.composed_frame();
            chain.discovery_active = true;
            saved
        };

        let result = self.run_detect_flash(channel_id, max_leds, &saved).await;

        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(chain) = state.get_mut(channel_id) {
                chain.discovery_active = false;
            }
        }
        result
    }

    async fn run_detect_flash(
        &self,
        channel_id: &str,
        max_leds: u32,
        saved: &[RgbColor],
    ) -> Result<()> {
        let red = vec![RgbColor { r: 255, g: 0, b: 0 }; max_leds as usize];
        let black = vec![RgbColor { r: 0, g: 0, b: 0 }; max_leds as usize];

        for _ in 0..3 {
            self.adapter.write_composed_frame(channel_id, &red).await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            self.adapter
                .write_composed_frame(channel_id, &black)
                .await?;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        if !saved.is_empty() {
            self.adapter.write_composed_frame(channel_id, saved).await?;
        }
        Ok(())
    }

    /// Replay a persisted link at startup. No broadcast; no persist — the
    /// caller already owns both responsibilities at boot time.
    pub async fn restore_link(
        self: &Arc<Self>,
        channel_id: &str,
        record: &ChainLinkRecord,
    ) -> Result<Arc<dyn Device>> {
        self.spawn_link(
            channel_id,
            &record.id,
            record.name.clone(),
            record.topology.clone(),
            record.led_count,
        )
        .await
    }

    fn spawn_child(
        self: &Arc<Self>,
        channel_id: &str,
        child_id: &str,
        name: String,
        topology: ZoneTopology,
        led_count: u32,
    ) -> Arc<GenericArgb> {
        let hub: Arc<dyn ChainHub> = self.clone();
        Arc::new(GenericArgb::new(
            child_id.to_string(),
            channel_id.to_string(),
            name,
            topology,
            led_count,
            hub,
        ))
    }
}

#[async_trait]
impl ChainHub for ChainHost {
    async fn write_chain_slice(
        &self,
        channel_id: &str,
        child_device_id: &str,
        colors: &[RgbColor],
    ) -> Result<()> {
        let outcome = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let chain = state
                .get_mut(channel_id)
                .ok_or_else(|| anyhow::anyhow!("unknown channel: {channel_id}"))?;
            if !chain.update_slot(child_device_id, colors) {
                anyhow::bail!(
                    "no chain link {child_device_id} on channel {channel_id}; refusing to write"
                );
            }
            if chain.discovery_active {
                return Ok(());
            }
            chain.composed_frame()
        };
        self.adapter
            .write_composed_frame(channel_id, &outcome)
            .await
    }

    fn link_name(&self, channel_id: &str, child_device_id: &str) -> Option<String> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let chain = state.get(channel_id)?;
        let idx = chain.find_index(child_device_id)?;
        Some(chain.links[idx].name.clone())
    }
}

pub fn channel_info(
    channel_id: &str,
    name: &str,
    max_leds: u32,
    state: &ChannelChainState,
) -> ChainableChannelInfo {
    ChainableChannelInfo {
        channel_id: channel_id.to_string(),
        name: name.to_string(),
        max_leds,
        links: state.links.iter().map(|l| l.info()).collect(),
    }
}

/// Returns a user-facing reason on failure.
pub fn validate_led_count(topology: &ZoneTopology, led_count: u32) -> Result<(), String> {
    if led_count == 0 {
        return Err("LED count must be at least 1".to_string());
    }
    match topology {
        ZoneTopology::Linear | ZoneTopology::Ring | ZoneTopology::Grid => Ok(()),
        ZoneTopology::Rings { count } => {
            let n = *count as u32;
            if n == 0 {
                return Err("Rings count must be at least 1".to_string());
            }
            if !led_count.is_multiple_of(n) {
                let prev = (led_count / n).saturating_mul(n);
                let next = (led_count / n).saturating_add(1).saturating_mul(n);
                Err(format!(
                    "Rings ×{n} requires the LED count to be a multiple of {n} \
                     (you have {led_count}; closest valid values are {prev} or {next})",
                ))
            } else {
                Ok(())
            }
        }
        ZoneTopology::Keyboard { .. } => {
            Err("keyboard topology is not valid for a chain link".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(id: &str, count: u32, locked: bool) -> ChainLinkRuntime {
        ChainLinkRuntime::new(
            id.to_string(),
            id.to_string(),
            ZoneTopology::Linear,
            count,
            locked,
        )
    }

    #[test]
    fn update_slot_writes_into_named_link() {
        let mut state = ChannelChainState::default();
        state.append(link("a", 3, false));
        state.append(link("b", 2, false));
        assert!(state.update_slot("b", &[RgbColor { r: 1, g: 2, b: 3 }; 2]));
        assert_eq!(state.links[1].last_colors.len(), 2);
    }

    #[test]
    fn composed_frame_pads_unwritten_links_with_black() {
        let mut state = ChannelChainState::default();
        state.append(link("a", 2, false));
        state.append(link("b", 3, false));
        state.update_slot("a", &[RgbColor { r: 10, g: 0, b: 0 }; 2]);
        let frame = state.composed_frame();
        assert_eq!(frame.len(), 5);
        assert_eq!(frame[0], RgbColor { r: 10, g: 0, b: 0 });
        assert_eq!(frame[1], RgbColor { r: 10, g: 0, b: 0 });
        assert_eq!(frame[2], RgbColor { r: 0, g: 0, b: 0 });
    }

    #[test]
    fn remove_locked_link_returns_err() {
        let mut state = ChannelChainState::default();
        state.append(link("a", 4, true));
        let err = state.remove("a").unwrap_err();
        assert!(err.contains("locked"));
    }

    #[test]
    fn reorder_cannot_move_unlocked_in_front_of_locked() {
        let mut state = ChannelChainState::default();
        state.append(link("locked", 4, true));
        state.append(link("u1", 4, false));
        state.append(link("u2", 4, false));
        state.reorder("u2", 0).unwrap();
        assert_eq!(state.links[0].child_id, "locked");
        assert_eq!(state.links[1].child_id, "u2");
    }

    #[test]
    fn used_leds_sums_all_link_counts() {
        let mut state = ChannelChainState::default();
        state.append(link("a", 24, true));
        state.append(link("b", 30, false));
        assert_eq!(state.used_leds(), 54);
    }

    #[test]
    fn composed_frame_concatenates_two_links_in_append_order() {
        let mut state = ChannelChainState::default();
        state.append(link("strip", 30, false));
        state.append(link("ring", 8, false));
        let red = RgbColor { r: 255, g: 0, b: 0 };
        let blue = RgbColor { r: 0, g: 0, b: 255 };
        state.update_slot("strip", &[red; 30]);
        state.update_slot("ring", &[blue; 8]);
        let frame = state.composed_frame();
        assert_eq!(frame.len(), 38);
        assert!(frame[..30].iter().all(|c| *c == red));
        assert!(frame[30..].iter().all(|c| *c == blue));
    }

    #[test]
    fn composed_frame_after_reorder_swaps_segments() {
        let mut state = ChannelChainState::default();
        state.append(link("a", 2, false));
        state.append(link("b", 3, false));
        let ca = RgbColor { r: 1, g: 0, b: 0 };
        let cb = RgbColor { r: 0, g: 1, b: 0 };
        state.update_slot("a", &[ca; 2]);
        state.update_slot("b", &[cb; 3]);
        state.reorder("b", 0).unwrap();
        let frame = state.composed_frame();
        assert_eq!(frame.len(), 5);
        assert_eq!(&frame[..3], &[cb; 3]);
        assert_eq!(&frame[3..], &[ca; 2]);
    }

    #[test]
    fn rename_rejects_locked_link() {
        let mut state = ChannelChainState::default();
        state.append(link("locked", 4, true));
        let err = state.rename("locked", "new").unwrap_err();
        assert!(err.contains("locked"));
    }

    #[test]
    fn rename_updates_unlocked_link() {
        let mut state = ChannelChainState::default();
        state.append(link("a", 4, false));
        state.rename("a", "Top Strip").unwrap();
        assert_eq!(state.links[0].name, "Top Strip");
    }

    #[test]
    fn validate_rings_count_rejects_non_divisible() {
        let topology = ZoneTopology::Rings { count: 3 };
        let err = validate_led_count(&topology, 8).unwrap_err();
        assert!(err.contains("multiple of 3"), "got: {err}");
    }

    #[test]
    fn validate_rings_count_accepts_divisible() {
        let topology = ZoneTopology::Rings { count: 3 };
        assert!(validate_led_count(&topology, 24).is_ok());
        assert!(validate_led_count(&topology, 9).is_ok());
    }

    #[test]
    fn validate_zero_count_rejected() {
        assert!(validate_led_count(&ZoneTopology::Linear, 0).is_err());
        assert!(validate_led_count(&ZoneTopology::Ring, 0).is_err());
    }

    #[test]
    fn validate_keyboard_rejected() {
        use halod_shared::types::{KeyboardFormFactor, KeyboardLayout};
        let topology = ZoneTopology::Keyboard {
            form_factor: KeyboardFormFactor::FullSize,
            layout: KeyboardLayout::Unknown,
        };
        assert!(validate_led_count(&topology, 100).is_err());
    }

    #[test]
    fn channel_info_carries_state_metadata_and_links() {
        let mut state = ChannelChainState::default();
        state.append(link("a", 24, true));
        state.append(link("b", 30, false));
        let info = channel_info("ch0", "Channel 1", 120, &state);
        assert_eq!(info.channel_id, "ch0");
        assert_eq!(info.links.len(), 2);
        assert!(info.links[0].locked);
        assert!(!info.links[1].locked);
    }

    struct StubAdapter {
        parent_id: String,
        channels: Vec<ChannelDescriptor>,
        last_written: Mutex<Vec<(String, Vec<RgbColor>)>>,
    }

    #[async_trait]
    impl ChainAdapter for StubAdapter {
        fn parent_id(&self) -> String {
            self.parent_id.clone()
        }
        fn channels(&self) -> Vec<ChannelDescriptor> {
            self.channels.clone()
        }
        async fn write_composed_frame(
            &self,
            channel_id: &str,
            composed: &[RgbColor],
        ) -> Result<()> {
            self.last_written
                .lock()
                .unwrap()
                .push((channel_id.to_string(), composed.to_vec()));
            Ok(())
        }
    }

    fn stub_parts() -> (Arc<ChainHost>, Arc<StubAdapter>) {
        let adapter = Arc::new(StubAdapter {
            parent_id: "parent_x".to_string(),
            channels: vec![ChannelDescriptor {
                channel_id: "a".to_string(),
                display_name: "Channel A".to_string(),
                max_leds: 120,
            }],
            last_written: Mutex::new(Vec::new()),
        });
        let host = ChainHost::new(adapter.clone());
        (host, adapter)
    }

    #[test]
    fn chainable_channels_reports_seeded_channels_with_empty_state() {
        let host = stub_parts().0;
        let info = host.chainable_channels();
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].channel_id, "a");
        assert!(info[0].links.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn detect_channel_writes_flash_sequence_to_adapter() {
        let (host, adapter) = stub_parts();

        let task = tokio::spawn({
            let host = host.clone();
            async move { host.detect_channel("a").await.unwrap() }
        });

        for _ in 0..3 {
            tokio::time::advance(Duration::from_millis(300)).await;
            tokio::time::advance(Duration::from_millis(200)).await;
        }
        task.await.unwrap();

        let written = adapter
            .last_written
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 3 on + 3 off = 6 writes; no restore (no links → saved frame is empty)
        assert_eq!(written.len(), 6);
        for (i, (ch, colors)) in written.iter().enumerate() {
            assert_eq!(ch, "a");
            assert_eq!(colors.len(), 120);
            if i % 2 == 0 {
                assert!(
                    colors.iter().all(|c| c.r == 255 && c.g == 0 && c.b == 0),
                    "write {i} should be red"
                );
            } else {
                assert!(
                    colors.iter().all(|c| *c == (RgbColor { r: 0, g: 0, b: 0 })),
                    "write {i} should be black"
                );
            }
        }
    }

    #[test]
    fn detect_channel_suppresses_slice_writes_during_discovery() {
        let (host, adapter) = stub_parts();

        {
            let mut state = host.state.lock().unwrap_or_else(|e| e.into_inner());
            let chain = state.get_mut("a").unwrap();
            chain.discovery_active = true;
            chain.links.push(ChainLinkRuntime::new(
                "x".into(),
                "x".into(),
                ZoneTopology::Linear,
                4,
                false,
            ));
        }

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            host.write_chain_slice("a", "x", &[RgbColor { r: 1, g: 2, b: 3 }; 4])
                .await
                .unwrap();
        });

        assert!(adapter
            .last_written
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
    }

    use crate::drivers::{CapabilityRef, RgbCapability, RgbStateSlot};
    use halod_shared::types::{LedPosition, RgbDescriptor, RgbState, RgbZone};

    /// Minimal RGB device with a single zone — enough to exercise
    /// `register_auto_link`, which reads the first zone's topology + led count.
    struct StubRgbDevice {
        id: String,
        desc: RgbDescriptor,
        rgb: RgbStateSlot,
    }

    impl StubRgbDevice {
        fn new(id: &str, leds: u32) -> Arc<Self> {
            Arc::new(Self {
                id: id.to_string(),
                desc: RgbDescriptor {
                    zones: vec![RgbZone {
                        id: "z0".to_string(),
                        name: "z0".to_string(),
                        topology: ZoneTopology::Linear,
                        leds: (0..leds)
                            .map(|i| LedPosition {
                                id: i as _,
                                x: 0.0,
                                y: 0.0,
                            })
                            .collect(),
                    }],
                    native_effects: vec![],
                },
                rgb: RgbStateSlot::default(),
            })
        }
    }

    #[async_trait]
    impl Device for StubRgbDevice {
        fn id(&self) -> &str {
            &self.id
        }
        fn name(&self) -> &str {
            &self.id
        }
        fn vendor(&self) -> &str {
            "stub"
        }
        fn model(&self) -> &str {
            "stub"
        }
        async fn initialize(&self) -> Result<bool> {
            Ok(true)
        }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
            vec![CapabilityRef::Rgb(self)]
        }
    }

    #[async_trait]
    impl RgbCapability for StubRgbDevice {
        fn descriptor(&self) -> &RgbDescriptor {
            &self.desc
        }
        async fn apply(&self, _: RgbState) -> Result<()> {
            Ok(())
        }
        async fn write_frame(&self, _: &str, _: &[RgbColor]) -> Result<()> {
            Ok(())
        }
        fn rgb_state(&self) -> &RgbStateSlot {
            &self.rgb
        }
    }

    #[tokio::test]
    async fn add_link_uses_supplied_name_not_child_default() {
        let host = stub_parts().0;
        let spec = ChainLinkSpec {
            name: "Top Strip".to_string(),
            topology: ZoneTopology::Linear,
            led_count: 10,
        };
        let (child_id, _child) = host.add_link("a", spec).await.unwrap();

        let name = host.link_name("a", &child_id);
        assert_eq!(name.as_deref(), Some("Top Strip"));
    }

    #[tokio::test]
    async fn auto_link_replace_keeps_children_consistent_with_link_state() {
        let host = stub_parts().0;

        let first: Arc<dyn Device> = StubRgbDevice::new("hw_a", 4);
        host.register_auto_link("a", first).await;

        // A re-detect replaces the locked slot-0 link with a fresh accessory.
        let second: Arc<dyn Device> = StubRgbDevice::new("hw_b", 4);
        host.register_auto_link("a", second).await;

        // Exactly one locked link, and `children()` must mirror it — not the
        // stale first Arc, not both.
        {
            let state = host.state.lock().unwrap_or_else(|e| e.into_inner());
            let links = &state.get("a").unwrap().links;
            assert_eq!(links.len(), 1);
            assert_eq!(links[0].child_id, "hw_b");
        }

        let child_ids: Vec<String> = host
            .children()
            .await
            .iter()
            .map(|c| c.id().to_owned())
            .collect();
        assert_eq!(child_ids, vec!["hw_b".to_string()]);
    }

    proptest::proptest! {
        /// Property: `validate_led_count` for `Rings { count }` returns `Ok`
        /// iff `led_count` is a multiple of `count`.
        #[test]
        fn rings_divisibility_is_exact(count in 1u8.., led_count in 1u32..=1000) {
            let topology = ZoneTopology::Rings { count };
            let result = validate_led_count(&topology, led_count);
            assert_eq!(result.is_ok(), led_count % count as u32 == 0);
        }
    }
}
