//! Vendor-agnostic chain machinery.
//!
//! Parent drivers that expose chainable ARGB channels (today: ASUS Aura USB,
//! NZXT Control Hub, NZXT Kraken — tomorrow: any controller with daisy-chained
//! ARGB) embed a [`ChainHost`] and implement [`ChainAdapter`]. The host owns
//! all chain state and CRUD; the adapter is the small vendor-specific surface
//! that just enumerates channels and writes composed frames to the wire.
//!
//! Children — instances of
//! [`crate::drivers::vendors::generic::devices::generic_argb::GenericArgb`] — talk to
//! the host through the [`ChainHub`] trait. The host concatenates every link's
//! last colors per channel and dispatches one composed frame per write.
//!
//! See `docs/chainable-argb.md` for the end-to-end walkthrough and the
//! "how to add a chainable driver" recipe.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Mutex as TokioMutex;

use halod_protocol::types::{
    ChainLinkInfo, ChainableChannelInfo, RgbColor, ZoneTopology,
};

use crate::config::ChainLinkRecord;
use crate::drivers::vendors::generic::devices::generic_argb::GenericArgb;
use crate::drivers::{ChainLinkKind, ChainLinkSpec, Device};
use crate::state::AppState;

// ── Per-link / per-channel runtime state ────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ChainLinkRuntime {
    pub child_id: String,
    pub name: String,
    pub topology: ZoneTopology,
    pub led_count: u32,
    /// True for hardware-detected accessories (e.g. an NZXT F-Fan). The lock
    /// keeps the hardware in sole control of the slot — no user mutation.
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

    /// Locked-link errors should reach the user — callers must surface them.
    pub fn remove(&mut self, child_id: &str) -> Result<bool, &'static str> {
        let Some(i) = self.find_index(child_id) else {
            return Ok(false);
        };
        if self.links[i].locked {
            return Err("chain link is locked");
        }
        self.links.remove(i);
        Ok(true)
    }

    pub fn rename(&mut self, child_id: &str, new_name: &str) -> Result<bool, &'static str> {
        let Some(i) = self.find_index(child_id) else {
            return Ok(false);
        };
        if self.links[i].locked {
            return Err("chain link is locked");
        }
        self.links[i].name = new_name.to_string();
        Ok(true)
    }

    pub fn reorder(&mut self, child_id: &str, new_index: usize) -> Result<bool, &'static str> {
        let Some(i) = self.find_index(child_id) else {
            return Ok(false);
        };
        if self.links[i].locked {
            return Err("chain link is locked");
        }
        let link = self.links.remove(i);
        let target = new_index.min(self.links.len());
        // Position 0 is reserved for the hardware-detected (locked) accessory.
        let target = if self
            .links
            .first()
            .map(|l| l.locked)
            .unwrap_or(false)
            && target == 0
        {
            1
        } else {
            target
        };
        self.links.insert(target, link);
        Ok(true)
    }
}

// ── Public traits & descriptor ──────────────────────────────────────────────

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
    /// Stable parent device id (used to seed new child ids).
    fn parent_id(&self) -> String;

    /// Enumerate every chainable channel this parent exposes. Order must be
    /// stable across calls — the UI renders channels in this order.
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

// ── ChainHost ────────────────────────────────────────────────────────────────

/// Shared chain runtime. Owns state + adapter + the Arc handles to spawned
/// children; drivers embed an `Arc<ChainHost>` and forward their
/// [`crate::drivers::ChainCapability`] impls here.
pub struct ChainHost {
    adapter: Arc<dyn ChainAdapter>,
    link_kind: ChainLinkKind,
    /// `std::sync::Mutex`: every operation holds the lock briefly without
    /// crossing `.await`, and `chainable_channels` (sync from the serializer)
    /// must not race against chain mutations.
    pub(crate) state: Mutex<HashMap<String, ChannelChainState>>,
    /// Arc handles to every device the host knows about (generic chain
    /// children + hardware-detected auto-locked accessories). Tokio mutex
    /// because the parent's async `serialize()` reads it.
    children: TokioMutex<Vec<Arc<dyn Device>>>,
}

impl ChainHost {
    /// Build a new host wrapping `adapter`. State is pre-seeded with empty
    /// chains for every channel the adapter reports.
    pub fn new(adapter: Arc<dyn ChainAdapter>, link_kind: ChainLinkKind) -> Arc<Self> {
        let mut state: HashMap<String, ChannelChainState> = HashMap::new();
        for d in adapter.channels() {
            state.insert(d.channel_id, ChannelChainState::default());
        }
        Arc::new(Self {
            adapter,
            link_kind,
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
        let state = self.state.lock().unwrap();
        channels
            .into_iter()
            .map(|d| {
                let empty = ChannelChainState::default();
                let chain = state.get(&d.channel_id).unwrap_or(&empty);
                channel_info(
                    &d.channel_id,
                    &d.display_name,
                    d.max_leds,
                    self.link_kind.as_str(),
                    chain,
                )
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
            child.id(),
            child.name().to_string(),
            zone.topology.clone(),
            zone.leds.len() as u32,
            /* locked */ true,
        );
        {
            let mut state = self.state.lock().unwrap();
            let chain = state.entry(channel_id.to_string()).or_default();
            // If position 0 already holds a locked link, replace it; else prepend.
            if let Some(first) = chain.links.first() {
                if first.locked {
                    chain.links[0] = link;
                    return;
                }
            }
            chain.links.insert(0, link);
        }
        // Track the Arc so the parent's serialize() can list it.
        let mut children = self.children.lock().await;
        let cid = child.id();
        children.retain(|c| c.id() != cid);
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
    /// - `spec.kind` must match the host's declared kind
    /// - the channel must exist
    /// - `led_count` must fit within remaining budget
    /// - `led_count` must satisfy the topology's divisibility constraint
    ///   (Rings)
    pub async fn add_link(
        self: &Arc<Self>,
        channel_id: &str,
        spec: ChainLinkSpec,
        app: Arc<AppState>,
    ) -> Result<String> {
        if spec.kind != self.link_kind {
            anyhow::bail!(
                "this parent accepts {} chain links, got {:?}",
                self.link_kind.as_str(),
                spec.kind
            );
        }
        validate_led_count(&spec.topology, spec.led_count)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let max_leds = self
            .max_leds(channel_id)
            .ok_or_else(|| anyhow::anyhow!("unknown chainable channel: {channel_id}"))?;

        let new_id = {
            let mut state = self.state.lock().unwrap();
            let chain = state.entry(channel_id.to_string()).or_default();
            let used = chain.used_leds();
            if used + spec.led_count > max_leds {
                anyhow::bail!(
                    "LED budget exceeded on channel {channel_id}: {used} + {} > {max_leds}",
                    spec.led_count
                );
            }
            let id = format!(
                "{}_chain_{channel_id}_{}",
                self.adapter.parent_id(),
                uuid::Uuid::new_v4()
            );
            chain.append(ChainLinkRuntime::new(
                id.clone(),
                spec.name.clone(),
                spec.topology.clone(),
                spec.led_count,
                false,
            ));
            id
        };

        let child = self.spawn_child(channel_id, &new_id, spec.name, spec.topology, spec.led_count);
        if let Err(e) = child.initialize().await {
            // Roll back the reserved slot so a failed init can't leak.
            let mut state = self.state.lock().unwrap();
            if let Some(chain) = state.get_mut(channel_id) {
                let _ = chain.remove(&new_id);
            }
            return Err(e);
        }
        let device: Arc<dyn Device> = child;
        self.children.lock().await.push(device.clone());
        app.devices.lock().await.push(device);
        Ok(new_id)
    }

    pub async fn remove_link(
        &self,
        channel_id: &str,
        child_id: &str,
        app: Arc<AppState>,
    ) -> Result<()> {
        {
            let mut state = self.state.lock().unwrap();
            let chain = state
                .get_mut(channel_id)
                .ok_or_else(|| anyhow::anyhow!("unknown channel: {channel_id}"))?;
            let removed = chain.remove(child_id).map_err(anyhow::Error::msg)?;
            if !removed {
                anyhow::bail!("chain link not found: {child_id}");
            }
        }
        self.children.lock().await.retain(|d| d.id() != child_id);
        app.devices.lock().await.retain(|d| d.id() != child_id);
        Ok(())
    }

    pub async fn rename_link(
        &self,
        channel_id: &str,
        child_id: &str,
        new_name: &str,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let chain = state
            .get_mut(channel_id)
            .ok_or_else(|| anyhow::anyhow!("unknown channel: {channel_id}"))?;
        let ok = chain.rename(child_id, new_name).map_err(anyhow::Error::msg)?;
        if !ok {
            anyhow::bail!("chain link not found: {child_id}");
        }
        Ok(())
    }

    pub async fn reorder_link(
        &self,
        channel_id: &str,
        child_id: &str,
        new_index: usize,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let chain = state
            .get_mut(channel_id)
            .ok_or_else(|| anyhow::anyhow!("unknown channel: {channel_id}"))?;
        let ok = chain
            .reorder(child_id, new_index)
            .map_err(anyhow::Error::msg)?;
        if !ok {
            anyhow::bail!("chain link not found: {child_id}");
        }
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
            let mut state = self.state.lock().unwrap();
            let chain = state
                .get_mut(channel_id)
                .ok_or_else(|| anyhow::anyhow!("unknown channel: {channel_id}"))?;
            let saved = chain.composed_frame();
            chain.discovery_active = true;
            saved
        };

        let result = self.run_detect_flash(channel_id, max_leds, &saved).await;

        {
            let mut state = self.state.lock().unwrap();
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
            self.adapter.write_composed_frame(channel_id, &black).await?;
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
        app: Arc<AppState>,
    ) -> Result<()> {
        validate_led_count(&record.topology, record.led_count)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let max_leds = self
            .max_leds(channel_id)
            .ok_or_else(|| anyhow::anyhow!("unknown chainable channel: {channel_id}"))?;

        {
            let mut state = self.state.lock().unwrap();
            let chain = state.entry(channel_id.to_string()).or_default();
            let used = chain.used_leds();
            if used + record.led_count > max_leds {
                anyhow::bail!(
                    "restore LED budget exceeded on channel {channel_id}: {used} + {} > {max_leds}",
                    record.led_count
                );
            }
            chain.append(ChainLinkRuntime::new(
                record.id.clone(),
                record.name.clone(),
                record.topology.clone(),
                record.led_count,
                false,
            ));
        }

        let child = self.spawn_child(
            channel_id,
            &record.id,
            record.name.clone(),
            record.topology.clone(),
            record.led_count,
        );
        child.initialize().await?;
        let device: Arc<dyn Device> = child;
        self.children.lock().await.push(device.clone());
        app.devices.lock().await.push(device);
        Ok(())
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
            let mut state = self.state.lock().unwrap();
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
        let state = self.state.lock().unwrap();
        let chain = state.get(channel_id)?;
        let idx = chain.find_index(child_device_id)?;
        Some(chain.links[idx].name.clone())
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

pub fn channel_info(
    channel_id: &str,
    name: &str,
    max_leds: u32,
    link_kind: &str,
    state: &ChannelChainState,
) -> ChainableChannelInfo {
    ChainableChannelInfo {
        channel_id: channel_id.to_string(),
        name: name.to_string(),
        max_leds,
        link_kind: link_kind.to_string(),
        links: state.links.iter().map(|l| l.info()).collect(),
    }
}

/// LED count must satisfy each topology's divisibility constraint. Returns a
/// user-facing reason on failure. Single source of truth — both UI and daemon
/// IPC dispatch call this.
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
            if led_count % n != 0 {
                Err(format!(
                    "Rings ×{n} requires the LED count to be a multiple of {n} \
                     (you have {led_count}; closest valid values are {} or {})",
                    (led_count / n) * n,
                    (led_count / n + 1) * n,
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
        ChainLinkRuntime::new(id.to_string(), id.to_string(), ZoneTopology::Linear, count, locked)
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
        state.update_slot("strip", &vec![red; 30]);
        state.update_slot("ring", &vec![blue; 8]);
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
        assert!(state.rename("a", "Top Strip").unwrap());
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
        use halod_protocol::types::{KeyboardFormFactor, KeyboardLayout};
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
        let info = channel_info("ch0", "Channel 1", 120, "generic_nzxt_argb", &state);
        assert_eq!(info.channel_id, "ch0");
        assert_eq!(info.links.len(), 2);
        assert!(info.links[0].locked);
        assert!(!info.links[1].locked);
    }

    // ── ChainHost end-to-end ────────────────────────────────────────────────

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
        let host = ChainHost::new(adapter.clone(), ChainLinkKind::GenericAuraArgb);
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

        let written = adapter.last_written.lock().unwrap();
        // 3 on + 3 off = 6 writes; no restore (no links → saved frame is empty)
        assert_eq!(written.len(), 6);
        for (i, (ch, colors)) in written.iter().enumerate() {
            assert_eq!(ch, "a");
            assert_eq!(colors.len(), 120);
            if i % 2 == 0 {
                assert!(colors.iter().all(|c| c.r == 255 && c.g == 0 && c.b == 0), "write {i} should be red");
            } else {
                assert!(colors.iter().all(|c| *c == (RgbColor { r: 0, g: 0, b: 0 })), "write {i} should be black");
            }
        }
    }

    #[test]
    fn detect_channel_suppresses_slice_writes_during_discovery() {
        let (host, adapter) = stub_parts();

        {
            let mut state = host.state.lock().unwrap();
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

        assert!(adapter.last_written.lock().unwrap().is_empty());
    }
}
