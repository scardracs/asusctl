use std::env;

use dmi_id::DMIID;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};

use crate::keyboard::AdvancedAuraType;
use crate::{AuraModeNum, AuraZone, PowerZones};

pub const ASUS_LED_MODE_CONF: &str = "/usr/share/asusd/aura_support.toml";
pub const ASUS_LED_MODE_USER_CONF: &str = "/etc/asusd/asusd_user_ledmodes.toml";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct LedSupportData {
    /// This can be many different types of name:
    /// - `/sys/class/dmi/id/board_name` (must use for laptops)
    /// - The device name from `lsusb`
    ///
    /// The laptop board_name is found via `cat /sys/class/dmi/id/board_name`,
    /// e.g `GU603ZW`. The match doesn't have to be the complete model
    /// number as it is typically broken down such:
    /// - GU = product
    /// - 603 = model/platform
    /// - Z = variant/year or perhaps dGPU model (such as RTX 3xxx)
    /// - W = possibly dGPU model (such as RTX 3060Ti)
    ///
    /// If using a device name the match is similar to the above where it can be
    /// partial, so `ASUSTek Computer, Inc. ROG STRIX Arion` can be `STRIX
    /// Arion` for short. Case insensitive.
    pub device_name: String,
    /// The product ID (usb only)
    /// Example of using a product ID is:
    /// ```ignore
    /// $ lsusb
    /// $ Bus 003 Device 003: ID 0b05:19b6 ASUSTek Computer, Inc. N-KEY Device
    /// ```
    /// here `19b6` is all that is required. Case insensitive.
    #[serde(default)]
    pub product_id: String,
    /// Keyboard or device LED layout, this is the name of the externally
    /// defined layout file. Optional, can be an empty string
    #[serde(default)]
    pub layout_name: String,
    /// If empty will default to `Static` mode
    pub basic_modes: Vec<AuraModeNum>,
    /// Available on some laptops. This is where the keyboard may be split in to
    /// 4 zones and may have a logo and lightbar.
    ///
    /// Ignored if empty.
    #[serde(default)]
    pub basic_zones: Vec<AuraZone>,
    /// `Zoned` or `PerKey`.
    // TODO: remove and use layouts only
    #[serde(default)]
    pub advanced_type: AdvancedAuraType,
    /// If empty will default to `Keyboard` power zone
    pub power_zones: Vec<PowerZones>,
}

impl LedSupportData {
    /// Find the data for the device. This function will check DMI info for
    /// matches against laptops first, then will proceed with matching the
    /// `device_name` if there are no DMI matches.
    pub fn get_data(product_id: &str) -> Self {
        let mut dmi = DMIID::new().unwrap_or_default();
        if let Ok(board_name) = env::var("BOARD_NAME") {
            dmi.board_name = board_name;
        }
        // let prod_family = dmi.product_family().expect("Could not get
        // product_family");

        if let Some(data) = LedSupportFile::load_from_support_db() {
            return data.match_device(&dmi.board_name, product_id);
        }
        info!("Using generic LED control for keyboard brightness only. No aura_support file found");
        let mut data = LedSupportData::default();
        data.power_zones.push(PowerZones::Keyboard);
        data
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LedSupportFile {
    #[serde(default)]
    pub device: Vec<LedSupportData>,
}

impl LedSupportFile {
    pub fn get(&self) -> &[LedSupportData] {
        &self.device
    }

    /// The list is stored in ordered format, so the iterator must be reversed
    /// to ensure we match to *whole names* first before doing a glob match
    fn match_device(&self, device_name: &str, product_id: &str) -> LedSupportData {
        for config in self.device.iter().rev() {
            if device_name.contains(&config.device_name) {
                info!("Matched to {}", config.device_name);
                if !config.product_id.is_empty() {
                    info!("Checking product ID");
                    if config.product_id == product_id {
                        info!("Matched to {}", config.product_id);
                        return config.clone();
                    } else {
                        continue;
                    }
                }
                return config.clone();
            }
        }
        warn!(
            "the aura_support.toml file has no entry for this model: {device_name}, {product_id}. \
             Using a default"
        );
        LedSupportData {
            device_name: device_name.to_owned(),
            product_id: product_id.to_owned(),
            layout_name: "Default".to_owned(),
            basic_modes: vec![AuraModeNum::Static],
            basic_zones: vec![],
            advanced_type: AdvancedAuraType::None,
            power_zones: vec![PowerZones::Keyboard],
        }
    }

    /// Load `LedSupportFile` from the `aura_support.toml` file at
    /// `/usr/share/asusd/aura_support.toml` and append with data from
    /// `/etc/asusd/asusd_user_ledmodes.toml` if that file is available.
    ///
    /// Returns `None` if neither file exists or does not parse correctly.
    pub fn load_from_support_db() -> Option<Self> {
        let mut loaded = false;
        let mut data = LedSupportFile::default();
        // Load user configs first so they are first to be checked
        if let Ok(file) = std::fs::read_to_string(ASUS_LED_MODE_USER_CONF) {
            if file.is_empty() {
                warn!("{} is empty", ASUS_LED_MODE_USER_CONF);
            } else {
                if let Ok(mut tmp) = toml::from_str::<LedSupportFile>(&file) {
                    data.device.append(&mut tmp.device);
                }
                info!(
                    "Loaded user-defined LED support data from {}",
                    ASUS_LED_MODE_USER_CONF
                );
            }
        }
        // Load and append the default LED support data
        if let Ok(file) = std::fs::read_to_string(ASUS_LED_MODE_CONF) {
            if file.is_empty() {
                warn!("{} is empty", ASUS_LED_MODE_CONF);
            } else {
                let mut tmp: LedSupportFile = toml::from_str(&file)
                    .map_err(|e| error!("{e}"))
                    .unwrap_or_else(|_| panic!("Could not deserialise {}", ASUS_LED_MODE_CONF));
                data.device.append(&mut tmp.device);
                loaded = true;
                info!(
                    "Loaded default LED support data from {}",
                    ASUS_LED_MODE_CONF
                );
            }
        }
        data.device
            .sort_by(|a, b| a.device_name.cmp(&b.device_name));

        if loaded {
            return Some(data);
        }

        // If the system-wide files were not found (typical in CI or
        // development environments), attempt to load the bundled
        // `data/aura_support.toml` from the crate so tests and local runs
        // behave the same as when the package is installed.
        // Attempt to load a bundled `aura_support.toml` included at compile time.
        // Using `include_str!` ensures the data is available regardless of
        // runtime `CARGO_MANIFEST_DIR` or CI environment differences.
        let bundled_buf = include_str!("../data/aura_support.toml");
        if !bundled_buf.is_empty() {
            if let Ok(tmp) = toml::from_str::<LedSupportFile>(bundled_buf) {
                data.device.append(&mut tmp.device.clone());
                data.device
                    .sort_by(|a, b| a.device_name.cmp(&b.device_name));
                info!("Loaded bundled LED support data (embedded)");
                return Some(data);
            } else {
                warn!("Could not parse embedded bundled data file");
            }
        }

        warn!("Does {} exist?", ASUS_LED_MODE_USER_CONF);
        None
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::PathBuf;

    use super::LedSupportData;
    use crate::aura_detection::{LedSupportFile, PowerZones};
    use crate::keyboard::{AdvancedAuraType, LedCode};
    use crate::{AuraModeNum, AuraZone};

    #[test]
    fn check_data_parse() {
        let led = LedSupportData {
            device_name: "Test".to_owned(),
            product_id: String::new(),
            layout_name: "ga401".to_owned(),
            basic_modes: vec![AuraModeNum::Static],
            basic_zones: vec![
                AuraZone::Key1,
                AuraZone::Logo,
                AuraZone::BarLeft,
            ],
            advanced_type: AdvancedAuraType::Zoned(vec![LedCode::LightbarRight]),
            power_zones: vec![
                PowerZones::Keyboard,
                PowerZones::RearGlow,
            ],
        };

        assert!(toml::to_string(&led).is_ok());
    }

    #[test]
    fn check_data_file_parse() {
        let mut data = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        data.push("data/aura_support.toml");

        let buf = std::fs::read_to_string(&data).unwrap();

        let tmp = toml::from_str::<LedSupportFile>(&buf).unwrap();

        // Ensure the data is sorted
        let mut tmp_sort = tmp.clone();
        tmp_sort
            .device
            .sort_by(|a, b| a.product_id.cmp(&b.product_id));
        tmp_sort
            .device
            .sort_by(|a, b| a.device_name.cmp(&b.device_name));
        for model in tmp_sort.device.iter_mut() {
            model.basic_modes.sort_by_key(|a| *a as u8);
        }
        if tmp != tmp_sort {
            let sorted = toml::to_string_pretty(&tmp_sort).unwrap();
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&data)
                .unwrap();
            file.write_all(sorted.as_bytes()).unwrap();
            panic!(
                "aura_support.toml not sorted, should be {sorted}. File rewritten with correct \
                 order, run test again"
            )
        }

        println!("TOML: {}", toml::to_string_pretty(&tmp).unwrap());
    }

    #[test]
    fn find_data_file_groups() {
        let mut data = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        data.push("data/aura_support.toml");

        let buf = std::fs::read_to_string(&data).unwrap();

        let tmp = toml::from_str::<LedSupportFile>(&buf).unwrap();

        let mut modes: HashMap<Vec<AuraModeNum>, Vec<String>> = HashMap::new();

        for entry in tmp.device {
            if let Some(modes) = modes.get_mut(&entry.basic_modes) {
                modes.push(entry.device_name);
            } else {
                modes.insert(entry.basic_modes, vec![entry.device_name]);
            }
        }
        dbg!(modes);
    }
}
