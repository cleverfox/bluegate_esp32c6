use embedded_storage_async::nor_flash::NorFlash;
use esp_println::println;
use heapless::String;
use sequential_storage::cache::NoCache;
use sequential_storage::map;

/// Flash storage range for settings (separate from keys storage)
/// Must be at least 2× erase size (2 × 4KB = 8KB minimum)
/// Using 8KB before the keys storage area
/// Keys use 0x3E0000..0x3F0000 (64KB)
const FLASH_RANGE: core::ops::Range<u32> = 0x3DE000..0x3E0000;

/// Maximum length for device name string
pub const MAX_NAME_LEN: usize = 64;

/// Special slot ID for device name string (uses slot 255)
const NAME_SLOT_ID: u8 = 255;

/// Configuration slots enum - add your settings here
/// The discriminant value is used as the slot ID in flash
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigSlot {
    Pad = 0,
    IOPolarity = 1,
    LampPreStart = 2,
    ConnTimeout = 3,
    AutoClose = 4,

    LeftOpenDelay = 8,
    LeftOpenDuration = 9,
    RightOpenDelay = 10,
    RightOpenDuration = 11,

    LeftCloseDelay = 12,
    LeftCloseDuration = 13,
    RightCloseDelay = 14,
    RightCloseDuration = 15,
}

impl ConfigSlot {
    /// Convert enum to slot ID
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Settings storage manager
pub struct ConfigStore<S: NorFlash> {
    flash: S,
}

impl<S: NorFlash> ConfigStore<S> {
    /// Create a new ConfigStore
    pub async fn new(flash: S) -> Self {
        Self { flash }
    }

    /// Get a setting by slot number, returns default if not set
    pub async fn get_slot(&mut self, slot: u8, default: u32) -> u32 {
        let mut cache = NoCache::new();
        let mut buf = [0u8; 128];

        let res = map::fetch_item::<u8, u32, _>(
                    &mut self.flash,
                    FLASH_RANGE,
                    &mut cache,
                    &mut buf,
                    &slot,
                )
                .await;
        println!("get slot {}: {:?}",slot,res);
        match res {
            Ok(Some(value)) => {
                value
            },
            Ok(None) => default,
            Err(reason) => {
                println!("ERROR: get slot {} failed: {:?}",slot,reason);
                default
            }
        }
    }

    /// Get a setting by ConfigSlot enum, returns default if not set
    pub async fn get(&mut self, slot: ConfigSlot, default: u32) -> u32 {
        self.get_slot(slot.as_u8(), default).await
    }

    /// Set a setting by slot number
    pub async fn set_slot(
        &mut self,
        slot: u8,
        value: u32,
    ) -> Result<(), sequential_storage::Error<S::Error>> {
        let mut cache = NoCache::new();
        let mut buf = [0u8; 32];

        let res = map::store_item(
            &mut self.flash,
            FLASH_RANGE,
            &mut cache,
            &mut buf,
            &slot,
            &value,
        )
        .await;
        println!("set slot {} = {}: {:?}",slot,value,res);
        res
    }

    /// Set a setting by ConfigSlot enum
    pub async fn set(
        &mut self,
        slot: ConfigSlot,
        value: u32,
    ) -> Result<(), sequential_storage::Error<S::Error>> {
        self.set_slot(slot.as_u8(), value).await
    }

    /// Get the device name, returns default if not set
    pub async fn get_name(&mut self, default: &str) -> String<MAX_NAME_LEN> {
        let mut cache = NoCache::new();
        let mut buf = [0u8; 128];

        match map::fetch_item::<u8, [u8; MAX_NAME_LEN], _>(
            &mut self.flash,
            FLASH_RANGE,
            &mut cache,
            &mut buf,
            &NAME_SLOT_ID,
        )
        .await
        {
            Ok(Some(bytes)) => {
                // Find null terminator or end of array
                let len = bytes.iter().position(|&b| b == 0).unwrap_or(MAX_NAME_LEN);
                let mut s: String<MAX_NAME_LEN> = String::new();
                if let Ok(str_slice) = core::str::from_utf8(&bytes[..len]) {
                    let _ = s.push_str(str_slice);
                }
                if s.is_empty() {
                    let _ = s.push_str(default);
                }
                s
            }
            Ok(None) => {
                let mut s: String<MAX_NAME_LEN> = String::new();
                let _ = s.push_str(default);
                s
            }
            Err(_) => {
                let mut s: String<MAX_NAME_LEN> = String::new();
                let _ = s.push_str(default);
                s
            }
        }
    }

    /// Set the device name
    pub async fn set_name(&mut self, name: &str) -> Result<(), sequential_storage::Error<S::Error>> {
        let mut cache = NoCache::new();
        let mut buf = [0u8; 128];

        // Convert string to fixed-size byte array with null terminator
        let mut bytes = [0u8; MAX_NAME_LEN];
        let len = name.len().min(MAX_NAME_LEN - 1);
        bytes[..len].copy_from_slice(&name.as_bytes()[..len]);
        // Rest is already zeroed (null terminated)

        map::store_item(
            &mut self.flash,
            FLASH_RANGE,
            &mut cache,
            &mut buf,
            &NAME_SLOT_ID,
            &bytes,
        )
        .await
    }

    /// Get mutable reference to flash for sharing with KeyStore
    pub fn flash(&mut self) -> &mut S {
        &mut self.flash
    }
}
