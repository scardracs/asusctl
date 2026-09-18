//! Tests for the GPU PCI detection and power status module.
//!
//! These tests cover the pure/deterministic parts of `rog_platform::gpu_pci`:
//! enum conversions, GPU classification, and default values. Hardware-dependent
//! functions (`Device::find`, `get_gpu_power_status`) are tested via integration
//! tests on machines with actual GPUs.

use rog_platform::gpu_pci::{GfxPower, GpuTelemetry, is_discrete_gpu};
use std::str::FromStr;

// ---------------------------------------------------------------------------
// GpuTelemetry – Default
// ---------------------------------------------------------------------------

#[test]
fn gpu_telemetry_default_values() {
    let telemetry = GpuTelemetry::default();
    assert_eq!(telemetry.igpu_temp, -1.0);
    assert_eq!(telemetry.igpu_usage, -1.0);
    assert_eq!(telemetry.dgpu_temp, -1.0);
    assert_eq!(telemetry.dgpu_usage, -1.0);
    assert_eq!(telemetry.dgpu_freq_mhz, -1.0);
    assert!(!telemetry.dgpu_suspended);
    assert!(!telemetry.dgpu_disabled);
}

// ---------------------------------------------------------------------------
// GfxPower – FromStr
// ---------------------------------------------------------------------------

#[test]
fn gfx_power_from_str_active() {
    assert_eq!(GfxPower::from_str("active").unwrap(), GfxPower::Active);
}

#[test]
fn gfx_power_from_str_active_case_insensitive() {
    assert_eq!(GfxPower::from_str("ACTIVE").unwrap(), GfxPower::Active);
    assert_eq!(GfxPower::from_str("Active").unwrap(), GfxPower::Active);
}

#[test]
fn gfx_power_from_str_suspended() {
    assert_eq!(
        GfxPower::from_str("suspended").unwrap(),
        GfxPower::Suspended
    );
    assert_eq!(
        GfxPower::from_str("suspending").unwrap(),
        GfxPower::Suspended
    );
}

#[test]
fn gfx_power_from_str_dgpu_disabled() {
    assert_eq!(
        GfxPower::from_str("dgpu_disabled").unwrap(),
        GfxPower::AsusDisabled
    );
}

#[test]
fn gfx_power_from_str_asus_mux_discreet() {
    assert_eq!(
        GfxPower::from_str("asus_mux_discreet").unwrap(),
        GfxPower::AsusMuxDiscreet
    );
}

#[test]
fn gfx_power_from_str_handles_whitespace() {
    assert_eq!(
        GfxPower::from_str("  suspended\n").unwrap(),
        GfxPower::Suspended
    );
    assert_eq!(GfxPower::from_str("\tactive ").unwrap(), GfxPower::Active);
}

#[test]
fn gfx_power_from_str_unknown_fallback() {
    assert_eq!(
        GfxPower::from_str("auto").unwrap(),
        GfxPower::Unknown,
        "unexpected kernel string should map to Unknown"
    );
    assert_eq!(
        GfxPower::from_str("unsupported").unwrap(),
        GfxPower::Unknown
    );
    assert_eq!(GfxPower::from_str("").unwrap(), GfxPower::Unknown);
    assert_eq!(GfxPower::from_str("garbage").unwrap(), GfxPower::Unknown);
}

// ---------------------------------------------------------------------------
// GfxPower – Display round-trip
// ---------------------------------------------------------------------------

#[test]
fn gfx_power_display_roundtrip() {
    let variants = [
        GfxPower::Active,
        GfxPower::Suspended,
        GfxPower::AsusDisabled,
        GfxPower::AsusMuxDiscreet,
        GfxPower::Unknown,
    ];
    for &variant in &variants {
        let s = variant.to_string();
        let parsed = GfxPower::from_str(&s).unwrap();
        assert_eq!(variant, parsed, "failed round-trip for {variant:?}");
    }
}

// ---------------------------------------------------------------------------
// GfxPower – Serde
// ---------------------------------------------------------------------------

#[test]
fn gfx_power_serde_roundtrip() {
    let variants = [
        GfxPower::Active,
        GfxPower::Suspended,
        GfxPower::AsusDisabled,
        GfxPower::AsusMuxDiscreet,
        GfxPower::Unknown,
    ];
    for &variant in &variants {
        let json = serde_json::to_string(&variant).unwrap();
        let deserialized: GfxPower = serde_json::from_str(&json).unwrap();
        assert_eq!(
            variant, deserialized,
            "serde round-trip failed for {variant:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// GfxPower – Default
// ---------------------------------------------------------------------------

#[test]
fn gfx_power_default_is_unknown() {
    assert_eq!(GfxPower::default(), GfxPower::Unknown);
}

// ---------------------------------------------------------------------------
// GfxPower – Copy / Clone
// ---------------------------------------------------------------------------

#[test]
fn gfx_power_copy_clone() {
    let a = GfxPower::Active;
    let b = a;
    let c = a;
    assert_eq!(a, b);
    assert_eq!(b, c);
}

// ---------------------------------------------------------------------------
// is_discrete_gpu – PCI topology, independent of NVIDIA driver flavour
// ---------------------------------------------------------------------------

#[test]
fn discrete_gpu_nvidia_always() {
    assert!(is_discrete_gpu("10DE:2520", None, 1));
    assert!(is_discrete_gpu("10DE:2820", Some(true), 1));
}

#[test]
fn discrete_gpu_hybrid_amd_nvidia() {
    // GA503R-style: Radeon 680M + RTX 3080
    assert!(!is_discrete_gpu("1002:1681", Some(true), 1));
    assert!(is_discrete_gpu("10DE:24DC", Some(false), 1));
}

#[test]
fn discrete_gpu_mux_amd_still_igpu() {
    // MUX discreet: NVIDIA is boot VGA; the single AMD APU stays the iGPU
    assert!(!is_discrete_gpu("1002:1681", Some(false), 1));
    assert!(is_discrete_gpu("10DE:24DC", Some(true), 1));
}

#[test]
fn discrete_gpu_dual_amd() {
    assert!(!is_discrete_gpu("1002:1681", Some(true), 2));
    assert!(is_discrete_gpu("1002:73DF", Some(false), 2));
}

#[test]
fn discrete_gpu_intel_plus_nvidia() {
    // One Intel iGPU at most, never discrete.
    assert!(!is_discrete_gpu("8086:A7A0", Some(true), 0));
    assert!(!is_discrete_gpu("8086:A7A0", Some(false), 0));
    assert!(is_discrete_gpu("10DE:28E0", Some(false), 0));
}
