use embedded_storage_async::nor_flash::NorFlash;
use heapless::Vec;
use sequential_storage::cache::NoCache;
use sequential_storage::map;

/// Maximum number of keys that can be stored
pub const STORE_KEYS: usize = 1024;

/// Flash storage range for keys
/// Each key needs ~48 bytes (33 bytes data + sequential_storage overhead)
/// For 1024 keys: 1024 * 48 = ~50KB, using 64KB (0x10000) for safety margin
/// Range: 0x3E0000..0x3F0000 (64KB)
/// Note: Settings use 0x3DF000..0x3E0000 (4KB before this range)
const FLASH_RANGE: core::ops::Range<u32> = 0x3E0000..0x3F0000;

/// Key for storing the key count in the map (u16 to support >255 keys)
const KEY_COUNT_ID: u16 = 0;
/// Starting ID for actual keys
const KEY_START_ID: u16 = 1;

/// Mask for key type flags (2 LSB bits)
const KEY_FLAGS_MASK: u8 = 0x03;

/// Key storage manager that holds keys in memory and persists to flash
pub struct KeyStore {
    keys: Vec<[u8; 33], STORE_KEYS>,
}

impl KeyStore {
    /// Create a new KeyStore and load existing keys from flash
    /// Returns the KeyStore and gives back flash ownership
    pub async fn new<S: NorFlash>(mut flash: S) -> (Self, S) {
        let keys = Self::load_from_flash(&mut flash).await;
        (Self { keys }, flash)
    }

    /// Load keys from flash storage
    async fn load_from_flash<S: NorFlash>(flash: &mut S) -> Vec<[u8; 33], STORE_KEYS> {
        let mut keys: Vec<[u8; 33], STORE_KEYS> = Vec::new();
        let mut cache = NoCache::new();
        let mut buf = [0u8; 64];

        let count: u16 = match map::fetch_item::<u16, u16, _>(
            flash,
            FLASH_RANGE,
            &mut cache,
            &mut buf,
            &KEY_COUNT_ID,
        )
        .await
        {
            Ok(Some(c)) => c,
            Ok(None) => 0,
            Err(_) => 0,
        };

        for i in 0..count {
            let key_id = KEY_START_ID.wrapping_add(i);
            match map::fetch_item::<u16, [u8; 33], _>(
                flash,
                FLASH_RANGE,
                &mut cache,
                &mut buf,
                &key_id,
            )
            .await
            {
                Ok(Some(key)) => {
                    let _ = keys.push(key);
                }
                Ok(None) => {}
                Err(_) => {}
            }
        }

        keys
    }

    /// Save all keys to flash storage
    async fn save_to_flash<S: NorFlash>(&self, flash: &mut S) -> Result<(), sequential_storage::Error<S::Error>> {
        let mut cache = NoCache::new();
        let mut buf = [0u8; 64];

        let count = self.keys.len() as u16;
        map::store_item::<u16, u16, _>(
            flash,
            FLASH_RANGE,
            &mut cache,
            &mut buf,
            &KEY_COUNT_ID,
            &count,
        )
        .await?;

        for (i, key) in self.keys.iter().enumerate() {
            let key_id = KEY_START_ID.wrapping_add(i as u16);
            map::store_item::<u16, [u8; 33], _>(
                flash,
                FLASH_RANGE,
                &mut cache,
                &mut buf,
                &key_id,
                key,
            )
            .await?;
        }

        Ok(())
    }

    /// Compare two keys: matches if 2 LSB bits of first byte and bytes 1..33 are equal
    fn keys_match(stored: &[u8; 33], provided: &[u8; 33]) -> bool {
        (stored[0] & KEY_FLAGS_MASK) == (provided[0] & KEY_FLAGS_MASK) && stored[1..] == provided[1..]
    }

    /// Add a key to the store and persist to flash
    /// Returns Ok(true) if added, Ok(false) if already exists or store is full
    pub async fn add<S: NorFlash>(&mut self, flash: &mut S, key: [u8; 33]) -> Result<bool, sequential_storage::Error<S::Error>> {
        // Check if key already exists
        for stored in &self.keys {
            if Self::keys_match(stored, &key) {
                return Ok(false);
            }
        }

        // Add to in-memory store
        if self.keys.push(key).is_err() {
            return Ok(false); // Store is full
        }

        // Persist to flash
        self.save_to_flash(flash).await?;
        Ok(true)
    }

    /// Delete a key from the store and persist to flash
    /// Compares using 2 LSB bits of first byte and bytes 1..33
    /// Returns Ok(true) if deleted, Ok(false) if not found
    pub async fn del<S: NorFlash>(&mut self, flash: &mut S, key: [u8; 33]) -> Result<bool, sequential_storage::Error<S::Error>> {
        let mut found_idx = None;

        for (i, stored) in self.keys.iter().enumerate() {
            if Self::keys_match(stored, &key) {
                found_idx = Some(i);
                break;
            }
        }

        match found_idx {
            Some(idx) => {
                self.keys.remove(idx);
                self.save_to_flash(flash).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Lookup a key in the store
    /// Compares using 2 LSB bits of first byte and bytes 1..33
    /// Returns the first byte (containing permissions in 6 MSB bits) if found, 0 if not found
    pub fn lookup(&self, key: &[u8; 33]) -> u8 {
        for stored in &self.keys {
            if Self::keys_match(stored, key) {
                return stored[0];
            }
        }
        0
    }

    /// Get the number of stored keys
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Check if the store is empty
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Get key by index
    /// Returns Some(key) if index is valid, None if out of range
    pub fn get(&self, index: usize) -> Option<&[u8; 33]> {
        self.keys.get(index)
    }
}
