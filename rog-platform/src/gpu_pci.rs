//! GPU PCI device detection and power status monitoring.
//!
//! Display GPUs are enumerated from sysfs (`/sys/bus/pci/devices`). NVIDIA is
//! always the dGPU; Intel, if present, is a single iGPU; AMD is the iGPU unless
//! two AMD display GPUs exist, in which case `boot_vga` selects the APU.
//! Telemetry prefers sysfs and falls back to NVML by PCI BDF only for an active
//! NVIDIA device (proprietary, open-rm, and DKMS share libnvidia-ml).

use std::fmt::Display;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;

use log::{info, trace};
use serde::{Deserialize, Serialize};
use zbus::zvariant::{OwnedValue, Type, Value};

use crate::error::{PlatformError, Result};

// --- ASUS-specific sysfs paths (reused from rog-platform) ---

// Both locations read the same WMI devstate and report the same values. The
// `asus-nb-wmi` attributes are deprecated in the kernel and are compiled out
// with CONFIG_ASUS_WMI_DEPRECATED_ATTRS=n, so firmware-attributes comes first.
const ASUS_DGPU_DISABLE_PATHS: [&str; 2] = [
    "/sys/class/firmware-attributes/asus-armoury/attributes/dgpu_disable/current_value",
    "/sys/devices/platform/asus-nb-wmi/dgpu_disable",
];
const ASUS_GPU_MUX_PATHS: [&str; 2] = [
    "/sys/class/firmware-attributes/asus-armoury/attributes/gpu_mux_mode/current_value",
    "/sys/devices/platform/asus-nb-wmi/gpu_mux_mode",
];

/// The first of `paths` that this machine actually has.
fn first_existing(paths: &[&str]) -> Option<PathBuf> {
    paths
        .iter()
        .map(Path::new)
        .find(|path| path.exists())
        .map(Path::to_path_buf)
}

/// Read an attribute whose value is a single digit.
fn read_digit(path: &Path) -> Result<u8> {
    let mut file = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|e| PlatformError::Read(path.to_string_lossy().to_string(), e))?;
    let mut buf = [0u8; 1];
    file.read_exact(&mut buf)
        .map_err(|e| PlatformError::Read(path.to_string_lossy().to_string(), e))?;
    Ok(buf[0])
}

/// Path of the ASUS dgpu_disable attribute, if this machine has one.
pub fn asus_dgpu_disable_path() -> Option<PathBuf> {
    first_existing(&ASUS_DGPU_DISABLE_PATHS)
}

/// Path of the ASUS gpu_mux_mode attribute, if this machine has one.
pub fn asus_gpu_mux_path() -> Option<PathBuf> {
    first_existing(&ASUS_GPU_MUX_PATHS)
}

/// Check if the ASUS dgpu_disable attribute exists.
pub fn asus_dgpu_disable_exists() -> bool {
    asus_dgpu_disable_path().is_some()
}

/// Read the ASUS dgpu_disable value.
pub fn asus_dgpu_disabled() -> Result<bool> {
    let path = asus_dgpu_disable_path().ok_or(PlatformError::NotSupported)?;
    Ok(read_digit(&path)? == b'1')
}

/// Check if the ASUS gpu_mux_mode attribute exists.
pub fn asus_gpu_mux_exists() -> bool {
    asus_gpu_mux_path().is_some()
}

/// Read the ASUS gpu_mux_mode value. Returns true if in discreet (dGPU) mode.
pub fn asus_gpu_mux_discreet() -> Result<bool> {
    let path = asus_gpu_mux_path().ok_or(PlatformError::NotSupported)?;
    // gpu_mux_mode: 0 = dGPU (discreet), 1 = Optimus (hybrid)
    Ok(read_digit(&path)? == b'0')
}

// --- GfxPower ---

/// The runtime power status of the discrete GPU.
#[derive(
    Debug, Default, Type, Value, OwnedValue, PartialEq, Eq, Copy, Clone, Serialize, Deserialize,
)]
pub enum GfxPower {
    Active,
    Suspended,
    AsusDisabled,
    AsusMuxDiscreet,
    #[default]
    Unknown,
}

impl FromStr for GfxPower {
    type Err = PlatformError;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_lowercase().trim() {
            "active" => GfxPower::Active,
            // "suspending" is a runtime-PM transition: treat it as asleep so
            // telemetry never touches hwmon/DRM/NVML mid-cycle.
            "suspended" | "suspending" => GfxPower::Suspended,
            "dgpu_disabled" => GfxPower::AsusDisabled,
            "asus_mux_discreet" => GfxPower::AsusMuxDiscreet,
            _ => GfxPower::Unknown,
        })
    }
}

impl From<&GfxPower> for &str {
    fn from(gfx: &GfxPower) -> &'static str {
        match gfx {
            GfxPower::Active => "active",
            GfxPower::Suspended => "suspended",
            GfxPower::AsusDisabled => "dgpu_disabled",
            GfxPower::AsusMuxDiscreet => "asus_mux_discreet",
            GfxPower::Unknown => "unknown",
        }
    }
}

impl Display for GfxPower {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s: &str = self.into();
        write!(f, "{}", s)
    }
}

// --- PCI GPU identification ---

/// Nvidia PCI vendor ID, as it appears in `vendor:device` strings.
const NVIDIA_PCI_VENDOR: &str = "10DE";
/// AMD PCI vendor ID, as it appears in `vendor:device` strings.
const AMD_PCI_VENDOR: &str = "1002";
/// Intel PCI vendor ID, as it appears in `vendor:device` strings.
const INTEL_PCI_VENDOR: &str = "8086";

const NVIDIA_VENDOR_ID: u32 = 0x10de;
const AMD_VENDOR_ID: u32 = 0x1002;
const INTEL_VENDOR_ID: u32 = 0x8086;
const PCI_DEVICES_PATH: &str = "/sys/bus/pci/devices";

/// True if a `vendor:device` id belongs to a GPU vendor handled here.
/// Ids are uppercase hex, matching sysfs formatted as `10DE:2520`.
pub fn is_gpu_vendor(pci_id: &str) -> bool {
    pci_id.starts_with(NVIDIA_PCI_VENDOR)
        || pci_id.starts_with(AMD_PCI_VENDOR)
        || pci_id.starts_with(INTEL_PCI_VENDOR)
}

fn pci_id_is_nvidia(pci_id: &str) -> bool {
    pci_id.starts_with(NVIDIA_PCI_VENDOR)
}

fn pci_id_is_intel(pci_id: &str) -> bool {
    pci_id.starts_with(INTEL_PCI_VENDOR)
}

fn pci_id_is_amd(pci_id: &str) -> bool {
    pci_id.starts_with(AMD_PCI_VENDOR)
}

fn is_gpu_vendor_id(vendor: u32) -> bool {
    matches!(vendor, NVIDIA_VENDOR_ID | AMD_VENDOR_ID | INTEL_VENDOR_ID)
}

fn is_display_class_id(class: u32) -> bool {
    class >> 16 == 0x03
}

fn read_sysfs_hex(path: &Path) -> Option<u32> {
    let text = fs::read_to_string(path).ok()?;
    u32::from_str_radix(
        text.trim()
            .trim_start_matches("0x")
            .trim_start_matches("0X"),
        16,
    )
    .ok()
}

fn pci_bdf(dev_path: &Path) -> Option<&str> {
    dev_path.file_name()?.to_str()
}

/// Read the kernel `boot_vga` flag for a PCI device (`1`, `0`, or missing).
pub fn read_boot_vga(dev_path: &Path) -> Option<bool> {
    match fs::read_to_string(dev_path.join("boot_vga")).ok()?.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// Classify a display-class PCI GPU as discrete.
///
/// - NVIDIA is always the dGPU.
/// - Intel is always the iGPU (one device at most; no Intel dGPU).
/// - AMD is the iGPU unless `amd_count > 1`, then `boot_vga` selects the APU.
pub fn is_discrete_gpu(pci_id: &str, boot_vga: Option<bool>, amd_count: usize) -> bool {
    if pci_id_is_nvidia(pci_id) {
        return true;
    }
    if pci_id_is_intel(pci_id) {
        return false;
    }
    amd_count > 1 && boot_vga != Some(true)
}

/// True if a PCI class code is a display controller (base class `0x03`).
///
/// Accepts udev (`30000`, `30200`) and sysfs (`0x030000`) spellings.
pub fn is_display_class(pci_class: &str) -> bool {
    let hex = pci_class
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    u32::from_str_radix(hex, 16).is_ok_and(is_display_class_id)
}

fn read_hwmon_temp(dir: &Path) -> Option<f32> {
    fs::read_to_string(dir.join("temp1_input"))
        .ok()?
        .trim()
        .parse::<f32>()
        .ok()
        .map(|t| t / 1000.0)
}

fn read_drm_busy(dir: &Path) -> Option<f32> {
    fs::read_to_string(dir.join("device/gpu_busy_percent"))
        .or_else(|_| fs::read_to_string(dir.join("gpu_busy_percent")))
        .ok()?
        .trim()
        .parse::<f32>()
        .ok()
}

/// NVML is reused for temp/usage/freq in one Active telemetry pass, then
/// dropped. `Nvml::init` opens NVIDIA device nodes and would otherwise pin
/// the card out of runtime PM sleep across polls.
static NVML: Mutex<Option<nvml_wrapper::Nvml>> = Mutex::new(None);

fn with_nvml<T>(read: impl FnOnce(&nvml_wrapper::Nvml) -> Option<T>) -> Option<T> {
    let mut slot = NVML.lock().ok()?;
    if slot.is_none() {
        *slot = nvml_wrapper::Nvml::init().ok();
    }
    read(slot.as_ref()?)
}

fn release_nvml() {
    if let Ok(mut slot) = NVML.lock() {
        *slot = None;
    }
}

fn read_nvml_temp(bdf: &str) -> Option<f32> {
    with_nvml(|nvml| {
        let device = nvml.device_by_pci_bus_id(bdf).ok()?;
        let temp = device
            .temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu)
            .ok()?;
        Some(temp as f32)
    })
}

fn read_nvml_usage(bdf: &str) -> Option<f32> {
    with_nvml(|nvml| {
        let device = nvml.device_by_pci_bus_id(bdf).ok()?;
        let rates = device.utilization_rates().ok()?;
        Some(rates.gpu as f32)
    })
}

/// Graphics clock in MHz from an amdgpu hwmon directory, which reports the
/// shader clock as `freq1_input` in Hz.
fn read_hwmon_freq(dir: &Path) -> Option<f32> {
    let hz = fs::read_to_string(dir.join("freq1_input"))
        .ok()?
        .trim()
        .parse::<f32>()
        .ok()?;
    // 0 means the node exists but has no reading, not an idle clock
    (hz > 0.0).then(|| hz / 1_000_000.0)
}

fn read_nvml_freq(bdf: &str) -> Option<f32> {
    with_nvml(|nvml| {
        let device = nvml.device_by_pci_bus_id(bdf).ok()?;
        let clock = device
            .clock_info(nvml_wrapper::enum_wrappers::device::Clock::Graphics)
            .ok()?;
        Some(clock as f32)
    })
}

// --- Device ---

/// A PCI GPU device.
#[derive(Clone, Debug)]
pub struct Device {
    /// Path to the device sysfs entry.
    dev_path: PathBuf,
    /// Whether this device is the discrete GPU.
    is_dgpu: bool,
    /// Vendor:Device PCI ID string.
    pci_id: String,
}

impl Device {
    pub fn dev_path(&self) -> &PathBuf {
        &self.dev_path
    }

    pub fn is_dgpu(&self) -> bool {
        self.is_dgpu
    }

    pub fn pci_id(&self) -> &str {
        &self.pci_id
    }

    /// Read a file underneath the sys object.
    fn read_file(path: PathBuf) -> Result<String> {
        fs::read_to_string(&path)
            .map_err(|e| PlatformError::Read(path.to_string_lossy().to_string(), e))
    }

    /// Read the runtime power status from sysfs.
    pub fn get_runtime_status(&self) -> Result<GfxPower> {
        let mut path = self.dev_path.clone();
        path.push("power");
        path.push("runtime_status");
        trace!("get_runtime_status: {path:?}");
        match Self::read_file(path) {
            Ok(inner) => GfxPower::from_str(inner.as_str()),
            // The device is gone or its runtime PM state is unreadable. `off` is
            // not a value runtime_status ever reports, so don't invent it.
            Err(_) => Ok(GfxPower::Unknown),
        }
    }

    /// True when touching this device's hwmon, DRM, or NVML could resume a
    /// sleeping or firmware-disabled discrete GPU.
    fn dgpu_must_stay_asleep(&self) -> bool {
        self.is_dgpu && (asus_dgpu_disabled().unwrap_or(false) || !self.runtime_is_active())
    }

    fn runtime_is_active(&self) -> bool {
        self.get_runtime_status().unwrap_or_default() == GfxPower::Active
    }

    /// True if a `/sys/class/{hwmon,drm}` entry belongs to this GPU, not an
    /// ancestor PCIe bridge.
    fn sysfs_belongs_to_this_gpu(&self, class_entry: &Path) -> bool {
        class_entry
            .join("device")
            .canonicalize()
            .ok()
            .is_some_and(|p| p == self.dev_path || p.starts_with(&self.dev_path))
    }

    fn read_matching_class_nodes(
        &self,
        class_dir: &str,
        read: fn(&Path) -> Option<f32>,
    ) -> Option<f32> {
        let entries = fs::read_dir(class_dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if self.sysfs_belongs_to_this_gpu(&path)
                && let Some(value) = read(&path)
            {
                return Some(value);
            }
        }
        None
    }

    fn nvml_fallback(&self, nvml: fn(&str) -> Option<f32>) -> Option<f32> {
        if !pci_id_is_nvidia(&self.pci_id) || !self.runtime_is_active() {
            return None;
        }
        nvml(pci_bdf(&self.dev_path)?)
    }

    /// Probe this device's hwmon directories with `read`, falling back to
    /// `nvml` on NVIDIA hardware that exposes no hwmon (proprietary, open-rm,
    /// and DKMS packages share libnvidia-ml; nouveau uses sysfs only).
    fn probe_hwmon(
        &self,
        read: fn(&Path) -> Option<f32>,
        nvml: fn(&str) -> Option<f32>,
    ) -> Option<f32> {
        if self.dgpu_must_stay_asleep() {
            return None;
        }

        if let Ok(entries) = fs::read_dir(self.dev_path.join("hwmon")) {
            for entry in entries.flatten() {
                if let Some(value) = read(&entry.path()) {
                    return Some(value);
                }
            }
        }

        self.read_matching_class_nodes("/sys/class/hwmon", read)
            .or_else(|| self.nvml_fallback(nvml))
    }

    /// Read the temperature (°C) of this GPU from sysfs hwmon with NVML fallback.
    pub fn get_temp(&self) -> Option<f32> {
        self.probe_hwmon(read_hwmon_temp, read_nvml_temp)
    }

    /// Probe this device's DRM directories for usage percentage, falling back to
    /// `nvml` on NVIDIA hardware that exposes no DRM usage node.
    fn probe_usage(&self, nvml: fn(&str) -> Option<f32>) -> Option<f32> {
        if self.dgpu_must_stay_asleep() {
            return None;
        }

        if let Some(busy) = read_drm_busy(&self.dev_path) {
            return Some(busy);
        }

        if let Ok(entries) = fs::read_dir(self.dev_path.join("drm")) {
            for entry in entries.flatten() {
                if let Some(busy) = read_drm_busy(&entry.path()) {
                    return Some(busy);
                }
            }
        }

        self.read_matching_class_nodes("/sys/class/drm", read_drm_busy)
            .or_else(|| self.nvml_fallback(nvml))
    }

    /// Read the GPU utilization percentage (0.0 - 100.0) from sysfs DRM nodes with NVML fallback.
    ///
    /// If this is a discrete GPU and it is not in the `Active` power state,
    /// this immediately returns `None` without accessing DRM sysfs or NVML to prevent
    /// waking the PCIe device from runtime PM sleep.
    pub fn get_usage_pct(&self) -> Option<f32> {
        self.probe_usage(read_nvml_usage)
    }

    /// Read the current graphics clock (MHz) of this GPU from sysfs hwmon with
    /// NVML fallback.
    pub fn get_freq_mhz(&self) -> Option<f32> {
        self.probe_hwmon(read_hwmon_freq, read_nvml_freq)
    }

    /// Enumerate PCI display GPUs from sysfs and classify each as iGPU or dGPU.
    pub fn find() -> Result<Vec<Self>> {
        Self::enumerate(Path::new(PCI_DEVICES_PATH))
    }

    /// Enumerate display GPUs under a `/sys/bus/pci/devices`-style directory.
    pub fn enumerate(pci_devices: &Path) -> Result<Vec<Self>> {
        let entries = fs::read_dir(pci_devices)
            .map_err(|err| PlatformError::Read(pci_devices.display().to_string(), err))?;

        let mut found = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(vendor) = read_sysfs_hex(&path.join("vendor")) else {
                continue;
            };
            let Some(class) = read_sysfs_hex(&path.join("class")) else {
                continue;
            };
            if !is_gpu_vendor_id(vendor) || !is_display_class_id(class) {
                continue;
            }

            let device = read_sysfs_hex(&path.join("device")).unwrap_or(0);
            let pci_id = format!("{vendor:04X}:{device:04X}");
            let dev_path = fs::canonicalize(&path).unwrap_or(path);
            let sysname = pci_bdf(&dev_path).unwrap_or_default().to_string();
            trace!("Looking at PCI device {sysname}");
            found.push(EnumeratedGpu {
                boot_vga: read_boot_vga(&dev_path),
                pci_id,
                sysname,
                dev_path,
            });
        }

        let amd_count = found
            .iter()
            .filter(|gpu| pci_id_is_amd(&gpu.pci_id))
            .count();

        Ok(found
            .into_iter()
            .map(|gpu| {
                let is_dgpu = is_discrete_gpu(&gpu.pci_id, gpu.boot_vga, amd_count);
                if is_dgpu {
                    info!("Found dgpu {} at {:?}", gpu.pci_id, gpu.sysname);
                } else {
                    info!("Found igpu {} at {:?}", gpu.pci_id, gpu.sysname);
                }
                Self {
                    dev_path: gpu.dev_path,
                    is_dgpu,
                    pci_id: gpu.pci_id,
                }
            })
            .collect())
    }
}

struct EnumeratedGpu {
    dev_path: PathBuf,
    pci_id: String,
    boot_vga: Option<bool>,
    sysname: String,
}

/// Get the current GPU power status, using all available detection methods.
///
/// This is the main entry point for determining dGPU power state. It tries:
/// 1. ASUS dgpu_disable attribute — writing 1 does not remove the device from
///    the PCI bus, so in integrated mode it must win over a still-enumerated
///    dGPU
/// 2. Direct PCI device detection (if dGPU devices are found)
/// 3. ASUS gpu_mux_mode attribute
pub fn get_gpu_power_status() -> GfxPower {
    power_status(&Device::find().unwrap_or_default())
}

fn power_status(devices: &[Device]) -> GfxPower {
    if asus_dgpu_disabled().unwrap_or(false) {
        return GfxPower::AsusDisabled;
    }

    if let Some(dgpu) = devices.iter().find(|d| d.is_dgpu()) {
        return dgpu.get_runtime_status().unwrap_or_default();
    }

    if asus_gpu_mux_discreet().unwrap_or(false) {
        return GfxPower::AsusMuxDiscreet;
    }

    GfxPower::Unknown
}

fn lookup_amdgpu_name(device_id: &str, revision: &str) -> Option<String> {
    let content = fs::read_to_string("/usr/share/libdrm/amdgpu.ids").ok()?;
    for line in content.lines().map(str::trim) {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split(',').map(str::trim).collect();
        if parts.len() >= 3
            && parts[0].eq_ignore_ascii_case(device_id)
            && parts[1].eq_ignore_ascii_case(revision)
            && !parts[2].is_empty()
        {
            return Some(parts[2].to_string());
        }
    }
    None
}

fn gpu_model_name(dev_path: &Path, pci_id: &str) -> String {
    let mut parts = pci_id.split(':');
    let vendor = parts.next().unwrap_or("");
    let device_id = parts.next().unwrap_or("");

    if vendor.eq_ignore_ascii_case(AMD_PCI_VENDOR) && !device_id.is_empty() {
        let revision = fs::read_to_string(dev_path.join("revision"))
            .unwrap_or_default()
            .trim()
            .trim_start_matches("0x")
            .to_lowercase();
        if let Some(name) = lookup_amdgpu_name(device_id, &revision) {
            return name;
        }
    }

    if let Ok(device) = udev::Device::from_syspath(dev_path)
        && let Some(model) = device.property_value("ID_MODEL_FROM_DATABASE")
    {
        let name = model.to_string_lossy();
        if !name.is_empty() {
            return name.into_owned();
        }
    }

    if pci_id.is_empty() {
        "Unknown GPU".to_string()
    } else {
        pci_id.to_string()
    }
}

pub fn get_gpu_names() -> (String, String) {
    let mut igpu = None;
    let mut dgpu = None;

    if let Ok(devices) = Device::find() {
        for device in devices {
            let name = gpu_model_name(device.dev_path(), device.pci_id());
            if device.is_dgpu() {
                dgpu = Some(name);
            } else {
                igpu = Some(name);
            }
        }
    }

    (
        igpu.unwrap_or_else(|| "Integrated GPU".to_string()),
        dgpu.unwrap_or_else(|| "Discrete GPU".to_string()),
    )
}

/// Telemetry metrics for both integrated and discrete GPUs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GpuTelemetry {
    pub igpu_temp: f32,
    pub igpu_usage: f32,
    pub dgpu_temp: f32,
    pub dgpu_usage: f32,
    pub dgpu_suspended: bool,
    pub dgpu_disabled: bool,
    pub dgpu_freq_mhz: f32,
}

impl Default for GpuTelemetry {
    fn default() -> Self {
        Self {
            igpu_temp: -1.0,
            igpu_usage: -1.0,
            dgpu_temp: -1.0,
            dgpu_usage: -1.0,
            dgpu_suspended: false,
            dgpu_disabled: false,
            dgpu_freq_mhz: -1.0,
        }
    }
}

/// Retrieve telemetry metrics for all detected GPUs in a single sysfs scan.
pub fn get_gpu_telemetry() -> GpuTelemetry {
    let mut telemetry = GpuTelemetry::default();
    let devices = Device::find().unwrap_or_default();
    let status = power_status(&devices);
    let dgpu_active = status == GfxPower::Active;
    telemetry.dgpu_suspended = status == GfxPower::Suspended;
    telemetry.dgpu_disabled = status == GfxPower::AsusDisabled;

    if !dgpu_active {
        release_nvml();
    }

    for device in devices {
        if device.is_dgpu() {
            if dgpu_active {
                telemetry.dgpu_temp = device.get_temp().unwrap_or(-1.0);
                telemetry.dgpu_usage = device.get_usage_pct().unwrap_or(-1.0);
                telemetry.dgpu_freq_mhz = device.get_freq_mhz().unwrap_or(-1.0);
            }
        } else {
            telemetry.igpu_temp = device.get_temp().unwrap_or(-1.0);
            telemetry.igpu_usage = device.get_usage_pct().unwrap_or(-1.0);
        }
    }

    // Release even when a probe returned None: with_nvml may have opened the
    // handle in this Active pass, and the next poll might already be asleep.
    if dgpu_active {
        release_nvml();
    }

    telemetry
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// A scratch directory unique to this test and process, removed on drop so
    /// nothing is left behind even when the test panics.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("{name}_{}", std::process::id()));
            fs::create_dir_all(&dir).expect("failed to create test dir");
            Self(dir)
        }

        fn join(&self, path: &str) -> PathBuf {
            self.0.join(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fake_device(dev_path: PathBuf) -> Device {
        Device {
            dev_path,
            is_dgpu: true,
            pci_id: "10DE:2820".to_string(),
        }
    }

    /// A dGPU that reads `active`, so the suspend guard does not short-circuit.
    fn fake_active_device(dev_path: PathBuf, pci_id: &str) -> Device {
        fs::create_dir_all(dev_path.join("power")).expect("failed to create power dir");
        fs::write(dev_path.join("power/runtime_status"), "active\n").expect("write");
        Device {
            dev_path,
            is_dgpu: true,
            pci_id: pci_id.to_string(),
        }
    }

    #[test]
    fn freq_reads_amdgpu_hwmon_hz() {
        let dir = TestDir::new("freq_amdgpu");
        fs::create_dir_all(dir.join("hwmon/hwmon1")).expect("mkdir");
        fs::write(dir.join("hwmon/hwmon1/freq1_input"), "2100000000\n").expect("write");
        let dev = fake_active_device(dir.0.clone(), "1002:1638");
        assert_eq!(dev.get_freq_mhz(), Some(2100.0));
    }

    #[test]
    fn freq_ignores_zero_hwmon_reading() {
        let dir = TestDir::new("freq_zero");
        fs::create_dir_all(dir.join("hwmon/hwmon1")).expect("mkdir");
        fs::write(dir.join("hwmon/hwmon1/freq1_input"), "0\n").expect("write");
        let dev = fake_active_device(dir.0.clone(), "1002:1638");
        assert_eq!(dev.get_freq_mhz(), None);
    }

    #[test]
    fn freq_none_when_no_nodes() {
        // AMD, so the NVML fallback is skipped and the result does not depend
        // on whether the test box has a live NVIDIA driver
        let dir = TestDir::new("freq_empty");
        let dev = fake_active_device(dir.0.clone(), "1002:1638");
        assert_eq!(dev.get_freq_mhz(), None);
    }

    #[test]
    fn gpu_vendor_matching() {
        assert!(is_gpu_vendor("10DE:2820"));
        assert!(is_gpu_vendor("1002:1638"));
        assert!(is_gpu_vendor("8086:A7A0"));
        assert!(!is_gpu_vendor("10EC:8168"));
        assert!(!is_gpu_vendor(""));
        // udev reports PCI_ID in uppercase hex, lowercase is not a valid input
        assert!(!is_gpu_vendor("10de:2820"));
    }

    #[test]
    fn nvidia_is_always_discrete() {
        assert!(is_discrete_gpu("10DE:2520", None, 0));
        assert!(is_discrete_gpu("10DE:2520", Some(true), 1));
        assert!(is_discrete_gpu("10DE:2520", Some(false), 0));
    }

    #[test]
    fn lone_amd_is_igpu_even_without_boot_vga() {
        assert!(!is_discrete_gpu("1002:1681", None, 1));
        assert!(!is_discrete_gpu("1002:1681", Some(false), 1));
    }

    #[test]
    fn intel_is_always_igpu() {
        assert!(!is_discrete_gpu("8086:A7A0", Some(true), 0));
        assert!(!is_discrete_gpu("8086:A7A0", Some(false), 0));
        assert!(!is_discrete_gpu("8086:A7A0", None, 1));
    }

    #[test]
    fn dual_amd_uses_boot_vga() {
        assert!(!is_discrete_gpu("1002:1681", Some(true), 2));
        assert!(is_discrete_gpu("1002:73DF", Some(false), 2));
        assert!(is_discrete_gpu("1002:73DF", None, 2));
    }

    #[test]
    fn read_boot_vga_parses_sysfs() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = TestDir::new("asusctl_test_boot_vga");
        assert_eq!(read_boot_vga(&dir.0), None);

        fs::write(dir.join("boot_vga"), "1\n")?;
        assert_eq!(read_boot_vga(&dir.0), Some(true));
        fs::write(dir.join("boot_vga"), "0")?;
        assert_eq!(read_boot_vga(&dir.0), Some(false));
        fs::write(dir.join("boot_vga"), "auto\n")?;
        assert_eq!(read_boot_vga(&dir.0), None);
        Ok(())
    }

    #[test]
    fn display_class_matching() {
        assert!(is_display_class("30000")); // VGA controller
        assert!(is_display_class("30200")); // 3D controller
        assert!(is_display_class("38000")); // other display controller
        assert!(is_display_class("0x030000")); // sysfs VGA controller
        assert!(is_display_class("0x030200")); // sysfs 3D controller
        assert!(!is_display_class("40300")); // Audio controller
        assert!(!is_display_class("040300")); // Audio controller with leading zero
        assert!(!is_display_class("20000")); // network controller
        assert!(!is_display_class("c0330")); // USB controller
        assert!(!is_display_class("3")); // base class alone is not a class code
        assert!(!is_display_class(""));
        assert!(!is_display_class("not-hex"));
    }

    #[test]
    fn first_existing_returns_first_present_path()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = TestDir::new("asusctl_test_first_existing");
        let real = dir.join("present");
        fs::write(&real, "1")?;
        let missing = dir.join("missing").to_string_lossy().to_string();
        let real_str = real.to_string_lossy().to_string();

        assert_eq!(
            first_existing(&[
                missing.as_str(),
                real_str.as_str()
            ]),
            Some(real.clone())
        );
        assert_eq!(
            first_existing(&[
                real_str.as_str(),
                missing.as_str()
            ]),
            Some(real)
        );
        assert_eq!(first_existing(&[missing.as_str()]), None);
        assert_eq!(first_existing(&[]), None);
        Ok(())
    }

    #[test]
    fn read_digit_reads_first_byte() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = TestDir::new("asusctl_test_read_digit");
        let attr = dir.join("current_value");

        fs::write(&attr, "1\n")?;
        assert_eq!(read_digit(&attr)?, b'1');
        fs::write(&attr, "0")?;
        assert_eq!(read_digit(&attr)?, b'0');

        fs::write(&attr, "")?;
        assert!(read_digit(&attr).is_err());
        assert!(read_digit(&dir.join("missing")).is_err());
        Ok(())
    }

    #[test]
    fn unreadable_runtime_status_is_unknown_not_off()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = TestDir::new("asusctl_test_runtime_status");
        fs::create_dir_all(dir.join("power"))?;
        let device = fake_device(dir.0.clone());

        // no runtime_status file at all: the device is gone, not powered off
        assert_eq!(device.get_runtime_status()?, GfxPower::Unknown);

        fs::write(dir.join("power/runtime_status"), "active\n")?;
        assert_eq!(device.get_runtime_status()?, GfxPower::Active);

        fs::write(dir.join("power/runtime_status"), "suspended\n")?;
        assert_eq!(device.get_runtime_status()?, GfxPower::Suspended);

        fs::write(dir.join("power/runtime_status"), "unsupported\n")?;
        assert_eq!(device.get_runtime_status()?, GfxPower::Unknown);
        Ok(())
    }

    #[test]
    fn device_get_temp_and_usage_when_suspended()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = TestDir::new("asusctl_test_temp_suspended");
        fs::create_dir_all(dir.join("power"))?;
        fs::write(dir.join("power/runtime_status"), "suspended\n")?;

        let hwmon_dir = dir.join("hwmon/hwmon0");
        fs::create_dir_all(&hwmon_dir)?;
        fs::write(hwmon_dir.join("temp1_input"), "55000\n")?;
        fs::write(dir.join("gpu_busy_percent"), "80\n")?;

        let device = fake_device(dir.0.clone());
        // Discrete GPU in suspended state must return None without querying hwmon/drm/nvml
        assert_eq!(device.get_temp(), None);
        assert_eq!(device.get_usage_pct(), None);
        Ok(())
    }

    #[test]
    fn device_get_temp_and_usage_when_active() -> std::result::Result<(), Box<dyn std::error::Error>>
    {
        let dir = TestDir::new("asusctl_test_temp_active");
        fs::create_dir_all(dir.join("power"))?;
        fs::write(dir.join("power/runtime_status"), "active\n")?;

        let hwmon_dir = dir.join("hwmon/hwmon0");
        fs::create_dir_all(&hwmon_dir)?;
        fs::write(hwmon_dir.join("temp1_input"), "62500\n")?;
        fs::write(dir.join("gpu_busy_percent"), "45\n")?;

        let device = fake_device(dir.0.clone());
        assert_eq!(device.get_temp(), Some(62.5));
        assert_eq!(device.get_usage_pct(), Some(45.0));
        Ok(())
    }

    #[test]
    fn device_get_temp_and_usage_non_dgpu_when_suspended()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = TestDir::new("asusctl_test_non_dgpu_suspended");
        fs::create_dir_all(dir.join("power"))?;
        fs::write(dir.join("power/runtime_status"), "suspended\n")?;

        let device = Device {
            dev_path: dir.0.clone(),
            is_dgpu: false,
            pci_id: "10DE:228E".to_string(),
        };

        fn panic_on_nvml(_bdf: &str) -> Option<f32> {
            panic!("NVML fallback must not be called when runtime_status is not Active");
        }

        // Non-dgpu Nvidia function without hwmon/drm in suspended state must not call NVML
        assert_eq!(device.probe_hwmon(read_hwmon_temp, panic_on_nvml), None);
        assert_eq!(device.probe_usage(panic_on_nvml), None);
        assert_eq!(device.probe_hwmon(read_hwmon_freq, panic_on_nvml), None);
        assert_eq!(device.get_temp(), None);
        assert_eq!(device.get_usage_pct(), None);
        assert_eq!(device.get_freq_mhz(), None);
        Ok(())
    }

    #[test]
    fn enumerate_hybrid_amd_nvidia_skips_audio()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = TestDir::new("asusctl_test_enum_hybrid");
        write_pci_gpu(
            &dir.0,
            "0000:04:00.0",
            "0x1002",
            "0x1681",
            "0x030000",
            Some("1"),
        )?;
        write_pci_gpu(
            &dir.0,
            "0000:01:00.0",
            "0x10de",
            "0x24dc",
            "0x030200",
            Some("0"),
        )?;
        write_pci_gpu(&dir.0, "0000:01:00.1", "0x10de", "0x228e", "0x040300", None)?;

        let devices = Device::enumerate(&dir.0)?;
        assert_eq!(devices.len(), 2);
        let igpu = devices.iter().find(|d| !d.is_dgpu()).expect("iGPU");
        let dgpu = devices.iter().find(|d| d.is_dgpu()).expect("dGPU");
        assert_eq!(igpu.pci_id(), "1002:1681");
        assert_eq!(dgpu.pci_id(), "10DE:24DC");
        Ok(())
    }

    #[test]
    fn enumerate_intel_igpu_plus_nvidia() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = TestDir::new("asusctl_test_enum_intel");
        write_pci_gpu(
            &dir.0,
            "0000:00:02.0",
            "0x8086",
            "0xa7a0",
            "0x030000",
            Some("1"),
        )?;
        write_pci_gpu(
            &dir.0,
            "0000:01:00.0",
            "0x10de",
            "0x28e0",
            "0x030200",
            Some("0"),
        )?;

        let devices = Device::enumerate(&dir.0)?;
        assert_eq!(devices.len(), 2);
        let igpu = devices.iter().find(|d| !d.is_dgpu()).expect("iGPU");
        let dgpu = devices.iter().find(|d| d.is_dgpu()).expect("dGPU");
        assert_eq!(igpu.pci_id(), "8086:A7A0");
        assert_eq!(dgpu.pci_id(), "10DE:28E0");
        Ok(())
    }

    #[test]
    fn enumerate_dual_amd_uses_boot_vga() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = TestDir::new("asusctl_test_enum_dual_amd");
        write_pci_gpu(
            &dir.0,
            "0000:05:00.0",
            "0x1002",
            "0x1681",
            "0x030000",
            Some("1"),
        )?;
        write_pci_gpu(
            &dir.0,
            "0000:01:00.0",
            "0x1002",
            "0x73df",
            "0x030000",
            Some("0"),
        )?;

        let devices = Device::enumerate(&dir.0)?;
        assert_eq!(devices.len(), 2);
        let igpu = devices.iter().find(|d| !d.is_dgpu()).expect("iGPU");
        let dgpu = devices.iter().find(|d| d.is_dgpu()).expect("dGPU");
        assert_eq!(igpu.pci_id(), "1002:1681");
        assert_eq!(dgpu.pci_id(), "1002:73DF");
        Ok(())
    }

    fn write_pci_gpu(
        root: &Path,
        bdf: &str,
        vendor: &str,
        device: &str,
        class: &str,
        boot_vga: Option<&str>,
    ) -> std::io::Result<()> {
        let dir = root.join(bdf);
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("vendor"), format!("{vendor}\n"))?;
        fs::write(dir.join("device"), format!("{device}\n"))?;
        fs::write(dir.join("class"), format!("{class}\n"))?;
        if let Some(value) = boot_vga {
            fs::write(dir.join("boot_vga"), format!("{value}\n"))?;
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires ASUS hardware with a dGPU"]
    fn live_dgpu_detection() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let devices = Device::find()?;
        println!("Found {} display devices:", devices.len());
        for dev in &devices {
            println!(
                "  - Device {} (is_dgpu: {}, path: {:?}, status: {:?})",
                dev.pci_id(),
                dev.is_dgpu(),
                dev.dev_path(),
                dev.get_runtime_status()?
            );
        }
        let (igpu_name, dgpu_name) = get_gpu_names();
        println!("GPU Names: iGPU = '{igpu_name}', dGPU = '{dgpu_name}'");
        let telemetry = get_gpu_telemetry();
        println!("Telemetry: {telemetry:?}");

        let dgpu = devices.iter().find(|d| d.is_dgpu()).expect("no dGPU found");
        assert!(is_gpu_vendor(dgpu.pci_id()));
        Ok(())
    }
}
