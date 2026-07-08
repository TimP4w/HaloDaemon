// SPDX-License-Identifier: MPL-2.0
// SPDX-FileCopyrightText: LibreHardwareMonitor contributors
// Derived from LibreHardwareMonitor's Amd17Cpu.cs
// (https://github.com/LibreHardwareMonitor/LibreHardwareMonitor).

#![cfg(target_os = "windows")]

//! AMD Ryzen (Zen, family 17h/19h/1Ah) CPU thermal decode.
//!
//! All temperatures live in the on-die SMN (System Management Network) and are
//! read through the [`AmdSmnBus`](crate::drivers::transports::amd_smn::AmdSmnBus).
//! This module holds only the pure register-decode math + CPUID-based model
//! detection so it can be unit-tested without hardware.
//!
//! Two thermal sources:
//!  * **Package Tctl/Tdie** — `THM_TCON_CUR_TMP`, the control temperature the
//!    cooler reacts to. Newer parts signal a 49 °C measurement offset via
//!    `RANGE_SEL`/`TJ_SEL`.
//!  * **Per-CCD Tdie** — one register per Core Complex Die. The base offset
//!    moved between Zen generations (Raphael/Granite Ridge use a different base
//!    than Matisse/Vermeer).

/// `THM_TCON_CUR_TMP` — package control temperature. CUR_TEMP is bits [31:21].
pub const F17H_M01H_THM_TCON_CUR_TMP: u32 = 0x0005_9800;
/// Per-CCD temperature base for Raphael (Zen 4, model 0x61) and Granite Ridge
/// (Zen 5, model 0x44). Successive CCDs are at `base + i * 4`.
pub const F17H_M61H_CCD1_TEMP: u32 = 0x0005_9B08;
/// Per-CCD temperature base for Matisse/Vermeer/Threadripper (Zen 2/3).
pub const F17H_M70H_CCD1_TEMP: u32 = 0x0005_9954;

/// Up to 8 CCDs are probed (Threadripper-class part counts).
pub const MAX_CCDS: u32 = 8;

const F17H_TEMP_RANGE_SEL_MASK: u32 = 0x0008_0000;
const F17H_TEMP_TJ_SEL_MASK: u32 = 0x0003_0000;

/// Decode the package Tctl/Tdie (°C) from a raw `THM_TCON_CUR_TMP` read.
///
/// CUR_TEMP[31:21] is a count of 0.125 °C steps. When the 49 °C offset flag is
/// set (via RANGE_SEL[19] or TJ_SEL[17:16]), 49 °C is subtracted. We expose the
/// combined "Core (Tctl/Tdie)" value: for Zen 2+ desktop parts Tctl and Tdie are
/// identical (no per-SKU negative offset), which covers Ryzen 7000/9000.
pub fn decode_tctl_tdie(raw: u32) -> f32 {
    let offset_flag = (raw & F17H_TEMP_RANGE_SEL_MASK) != 0
        || (raw & F17H_TEMP_TJ_SEL_MASK) == F17H_TEMP_TJ_SEL_MASK;
    let mut t = (raw >> 21) as f32 * 0.125;
    if offset_flag {
        t -= 49.0;
    }
    t
}

/// Decode one per-CCD Tdie (°C), or `None` if the CCD is unpopulated/invalid.
///
/// The raw value is 12 bits ([11:0]); each step is 0.125 °C with a fixed
/// −305 °C bias. A zero raw value means "no CCD here"; a result ≥ 125 °C is
/// rejected as out of range.
pub fn decode_ccd_temp(raw: u32) -> Option<f32> {
    let raw = raw & 0xFFF;
    if raw == 0 {
        return None;
    }
    let t = raw as f32 * 0.125 - 305.0;
    (t < 125.0).then_some(t)
}

/// SMN base offset of the first CCD temperature register for `model`.
pub fn ccd_temp_base(model: u8) -> u32 {
    if matches!(model, 0x61 | 0x44) {
        F17H_M61H_CCD1_TEMP
    } else {
        F17H_M70H_CCD1_TEMP
    }
}

/// Whether `model` exposes per-CCD temperature registers. Matches the set
/// LibreHardwareMonitor enables: Threadripper 3000 (0x31), Zen 2 (0x71),
/// Zen 3 (0x21), Zen 4 (0x61), Zen 5 (0x44).
pub fn supports_per_ccd(model: u8) -> bool {
    matches!(model, 0x31 | 0x71 | 0x21 | 0x61 | 0x44)
}

/// Aggregate per-CCD temperatures into `(max, average)`. Returns `None` for
/// fewer than two CCDs — the max/average sensors are only meaningful on
/// multi-CCD parts (a single CCD already equals its own max/average).
pub fn ccd_aggregate(temps: &[f32]) -> Option<(f32, f32)> {
    if temps.len() < 2 {
        return None;
    }
    let max = temps.iter().copied().fold(f32::MIN, f32::max);
    let avg = temps.iter().sum::<f32>() / temps.len() as f32;
    Some((max, avg))
}

/// Friendly Zen-generation label for a CPU family.
pub fn arch_label(family: u8) -> &'static str {
    match family {
        0x17 => "Zen / Zen+ / Zen 2",
        0x19 => "Zen 3 / Zen 4",
        0x1A => "Zen 5",
        _ => "Zen",
    }
}

/// Detect an AMD Zen CPU via CPUID. Returns `(family, model)` when the vendor
/// is AuthenticAMD and the family is one the SMN thermal path supports
/// (17h/19h/1Ah), otherwise `None`.
pub fn detect_amd_zen() -> Option<(u8, u8)> {
    #[cfg(target_arch = "x86_64")]
    {
        let (family, model) = unsafe { amd_signature()? };
        matches!(family, 0x17 | 0x19 | 0x1A).then_some((family, model))
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        None
    }
}

/// Read CPUID and return `(family, model)` if the vendor is AuthenticAMD.
///
/// # Safety
/// `__cpuid` is always available on x86_64; calling it has no preconditions.
#[cfg(target_arch = "x86_64")]
unsafe fn amd_signature() -> Option<(u8, u8)> {
    use std::arch::x86_64::__cpuid;

    // Leaf 0: vendor string in EBX,EDX,ECX = "Auth" "enti" "cAMD".
    let leaf0 = __cpuid(0);
    if leaf0.ebx != 0x6874_7541 || leaf0.edx != 0x6974_6E65 || leaf0.ecx != 0x444D_4163 {
        return None;
    }

    // Leaf 1 EAX: base/extended family and model fields.
    let eax = __cpuid(1).eax;
    let base_family = (eax >> 8) & 0xF;
    let ext_family = (eax >> 20) & 0xFF;
    let base_model = (eax >> 4) & 0xF;
    let ext_model = (eax >> 16) & 0xF;

    let family = if base_family == 0xF {
        base_family + ext_family
    } else {
        base_family
    };
    // Extended model is folded in for family 6 and 15 (all Zen are 15-derived).
    let model = if base_family == 0xF || base_family == 0x6 {
        (ext_model << 4) | base_model
    } else {
        base_model
    };

    Some((family as u8, model as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tctl_tdie_no_offset() {
        // 50 °C with no offset: 50 / 0.125 = 400 steps in bits [31:21].
        let raw = 400u32 << 21;
        assert_eq!(decode_tctl_tdie(raw), 50.0);
    }

    #[test]
    fn tctl_tdie_with_range_sel_offset() {
        // Same 400 steps but RANGE_SEL set → 49 °C subtracted.
        let raw = (400u32 << 21) | F17H_TEMP_RANGE_SEL_MASK;
        assert_eq!(decode_tctl_tdie(raw), 1.0);
    }

    #[test]
    fn tctl_tdie_with_tj_sel_offset() {
        // TJ_SEL[17:16] both set also signals the 49 °C offset.
        let raw = (400u32 << 21) | F17H_TEMP_TJ_SEL_MASK;
        assert_eq!(decode_tctl_tdie(raw), 1.0);
    }

    #[test]
    fn ccd_temp_valid() {
        // 60 °C: (60 + 305) / 0.125 = 2920.
        assert_eq!(decode_ccd_temp(2920), Some(60.0));
    }

    #[test]
    fn ccd_temp_zero_is_unpopulated() {
        assert_eq!(decode_ccd_temp(0), None);
    }

    #[test]
    fn ccd_temp_masks_to_12_bits() {
        // Upper bits beyond [11:0] are ignored.
        assert_eq!(decode_ccd_temp(0xF000 | 2920), Some(60.0));
    }

    #[test]
    fn ccd_temp_out_of_range_rejected() {
        // 125 °C is not < 125 → rejected. raw = (125 + 305) / 0.125 = 3440.
        assert_eq!(decode_ccd_temp(3440), None);
    }

    #[test]
    fn ccd_base_matches_model() {
        assert_eq!(ccd_temp_base(0x44), F17H_M61H_CCD1_TEMP); // Zen 5
        assert_eq!(ccd_temp_base(0x61), F17H_M61H_CCD1_TEMP); // Zen 4
        assert_eq!(ccd_temp_base(0x21), F17H_M70H_CCD1_TEMP); // Zen 3
        assert_eq!(ccd_temp_base(0x71), F17H_M70H_CCD1_TEMP); // Zen 2
    }

    #[test]
    fn per_ccd_support_set() {
        for m in [0x31, 0x71, 0x21, 0x61, 0x44] {
            assert!(
                supports_per_ccd(m),
                "model 0x{m:02X} should support per-CCD"
            );
        }
        for m in [0x00, 0x11, 0x40, 0x50] {
            assert!(!supports_per_ccd(m), "model 0x{m:02X} should not");
        }
    }

    #[test]
    fn aggregate_needs_two_ccds() {
        assert_eq!(ccd_aggregate(&[]), None);
        assert_eq!(ccd_aggregate(&[60.0]), None);
        let (max, avg) = ccd_aggregate(&[60.0, 70.0]).unwrap();
        assert_eq!(max, 70.0);
        assert_eq!(avg, 65.0);
    }

    #[test]
    fn aggregate_three_ccds() {
        let (max, avg) = ccd_aggregate(&[50.0, 60.0, 70.0]).unwrap();
        assert_eq!(max, 70.0);
        assert_eq!(avg, 60.0);
    }
}
