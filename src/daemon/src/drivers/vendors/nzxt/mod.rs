pub mod devices;
pub mod protocols;

/// Emit `arc_self_chain_hub` and `arc_self_fan_hub` for an NZXT device that
/// carries `chain_host: OnceLock<Arc<ChainHost>>` and `self_ref: Weak<Self>`.
macro_rules! impl_nzxt_chain_host_methods {
    () => {
        fn arc_self_chain_hub(&self) -> Arc<dyn crate::drivers::chain::ChainHub> {
            self.chain_host
                .get()
                .expect("chain_host not yet set")
                .clone()
        }

        fn arc_self_fan_hub(&self) -> Arc<dyn crate::drivers::FanHub> {
            self.self_ref
                .upgrade()
                .expect("arc_self_fan_hub called after device drop")
        }
    };
}
pub(crate) use impl_nzxt_chain_host_methods;
