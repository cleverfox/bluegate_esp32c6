use crate::keys::KeyStore;
use crate::settings::{ConfigStore, MAX_NAME_LEN};
use crate::types::FsmCommand;
use core::default::Default;
use crate::settings::ConfigSlot;
use core::option::Option;
use core::result::Result::{self, Err, Ok};
use ed25519_dalek::{Verifier, VerifyingKey};
use embassy_futures::join::join;
use embassy_futures::select::select;
// use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Sender;
use embassy_time::{Instant, Timer};
use embedded_storage_async::nor_flash::NorFlash;
use heapless::String;
use hex_fmt::HexFmt;
use p256::ecdsa::signature::hazmat::PrehashVerifier;
use rand_core::{CryptoRng, RngCore};
use sha2::{Digest, Sha256};
use trouble_host::prelude::*;

/// Max number of connections
const CONNECTIONS_MAX: usize = 2;

/// Max number of L2CAP channels.
const L2CAP_CHANNELS_MAX: usize = 2; // Signal + att

use esp_println::println;
const AUTH_LOG_CAP: usize = 100;
const AUTH_LOG_ENTRY_LEN: usize = 50;

#[derive(Copy, Clone)]
struct AuthLogEntry {
    pubkey: [u8; 33],
    uptime_ms: u64,
    addr: [u8; 6],
    auth_action: u16,
    success: bool,
}
impl Default for AuthLogEntry {
    fn default() -> Self {
        AuthLogEntry {
            pubkey: [0; 33],
            uptime_ms: 0,
            addr: [0; 6],
            auth_action: 0,
            success: false,
        }
    }
}


struct AuthLog {
    entries: [AuthLogEntry; AUTH_LOG_CAP],
    write_idx: usize,
    count: usize,
}

impl AuthLog {
    fn new() -> Self {
        Self {
            entries: [AuthLogEntry::default(); AUTH_LOG_CAP],
            write_idx: 0,
            count: 0,
        }
    }

    fn push(&mut self, entry: AuthLogEntry) {
        self.entries[self.write_idx] = entry;
        self.write_idx = (self.write_idx + 1) % AUTH_LOG_CAP;
        if self.count < AUTH_LOG_CAP {
            self.count += 1;
        }
    }

    fn count(&self) -> usize {
        self.count
    }

    fn get(&self, index: usize) -> Option<AuthLogEntry> {
        if index >= self.count {
            return None;
        }
        let newest_idx = if self.write_idx == 0 {
            AUTH_LOG_CAP - 1
        } else {
            self.write_idx - 1
        };
        let pos = (newest_idx + AUTH_LOG_CAP - index) % AUTH_LOG_CAP;
        Some(self.entries[pos])
    }

    fn entry_bytes(&self, index: usize) -> [u8; AUTH_LOG_ENTRY_LEN] {
        let mut out = [0u8; AUTH_LOG_ENTRY_LEN];
        if let Some(entry) = self.get(index) {
            // Flags: bit0 = valid, bit1 = success
            out[0] = 0x01 | if entry.success { 0x02 } else { 0x00 };
            out[1..34].copy_from_slice(&entry.pubkey);
            out[34..42].copy_from_slice(&entry.uptime_ms.to_le_bytes());
            out[42..48].copy_from_slice(&entry.addr);
            out[48..50].copy_from_slice(&entry.auth_action.to_le_bytes());
        }
        out
    }
}
// GATT Server definition
#[gatt_server]
struct Server {
    // battery_service: BatteryService,
    gate: GateService,
    // keys: Vec<[u8; 33]>,
}

#[gatt_service(uuid = "6a7e6a7e-4929-42d0-0000-fcc5a35e13f1")]
struct GateService {
    #[characteristic( uuid = "0100", read, value = [15;32] )]
    nonce: [u8; 32],

    #[characteristic(uuid = "0101", write, write_without_response, value = [0; 64])]
    authenticate: [u8; 64],

    #[characteristic(uuid = "0102", write, read, value = [255;33])]
    client_pubkey: [u8; 33],

    #[characteristic(uuid = "0103", write)]
    client_nonce: [u8; 32],

    #[characteristic(uuid = "0104", read, notify, value = false)]
    client_key_ack: bool,

    #[characteristic(uuid = "0105", read, notify, value = false)]
    authenticate_ack: bool,

    #[characteristic(uuid = "0106", read, write, value = 0)]
    auth_action: u16,

    #[characteristic(uuid = "0108", read, value = 0)]
    perm: u8,

    #[characteristic(uuid = "1100", write, value=0)]
    management: u8,

    #[characteristic(uuid = "1101", read, write, value=[0;33])]
    management_key: [u8;33],

    #[characteristic(uuid = "1102", read, write, value=0)]
    management_param_id: u8,

    #[characteristic(uuid = "1103", read, write, value=[0;4])]
    management_param_val: [u8;4],

    #[characteristic(uuid = "1104", read, write, value=[0;64])]
    management_name: [u8; 64],

    #[characteristic(uuid = "1105", read, notify, value = 0)]
    management_result: u8,

    #[characteristic(uuid = "1200", read, write, value = 0)]
    log_index: u16,

    #[characteristic(uuid = "1201", read, value = [0; AUTH_LOG_ENTRY_LEN])]
    log_entry: [u8; AUTH_LOG_ENTRY_LEN],

    #[characteristic(uuid = "1202", read, value = 0)]
    log_count: u16,
}

/// Admin permission flag (MSB high means admin)
const PERM_ADMIN: u8 = 0x80;
const PERM_ADMADMIN: u8 = 0x40;
const PERM_SETADMIN: u8 = 0x20;

/// Management action codes
const MGMT_ADD_KEY: u8 = 0x01;
const MGMT_DEL_KEY: u8 = 0x02;
const MGMT_GET_KEY: u8 = 0x03;
const MGMT_SET_PARAM: u8 = 0x10;
const MGMT_GET_PARAM: u8 = 0x11;
const MGMT_SET_NAME: u8 = 0x20;

/// Management result codes
const MGMT_OK: u8 = 0x00;
const MGMT_ERR_NOT_ADMIN: u8 = 0x01;
const MGMT_ERR_FLASH: u8 = 0x02;
const MGMT_ERR_NOT_FOUND: u8 = 0x03;
const MGMT_ERR_INVALID: u8 = 0x04;

// Run the BLE stack.
pub async fn run<C, RNG, S>(
    controller: C,
    rng: &mut RNG,
    name: &String<MAX_NAME_LEN>,
    mut keys: KeyStore,
    mut config: ConfigStore<S>,
    tx: Sender<'_, CriticalSectionRawMutex, FsmCommand, 4>,
    cfg_prog_mode: bool,
) where
    C: Controller,
    RNG: RngCore + CryptoRng,
    S: NorFlash,
{
    // Using a fixed "random" address can be useful for testing. In real scenarios, one would
    // use e.g. the MAC 6 byte array as the address (how to get that varies by the platform).
    let address: Address = Address::random([0xff, 0x8f, 0x1a, 0x05, 0xe4, 0xff]);
    println!("Our address = {:?}", address);

    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
        HostResources::new();
    let stack = trouble_host::new(controller, &mut resources).set_random_address(address);
    let Host {
        mut peripheral,
        runner,
        ..
    } = stack.build();

    println!("Starting advertising and GATT service");
    // let name = "BlueGate Lenina 66";
    let server = Server::new_with_config(GapConfig::Peripheral(PeripheralConfig {
        name,
        appearance: &appearance::access_control::ENTRANCE_GATE,
    }))
    .unwrap();

    let mut auth_log = AuthLog::new();

    // let rng: SeedableRng = SeedableRng::seed_from_u64(1234);
    let _ = join(ble_task(runner), async {
        loop {
            match advertise(name, &mut peripheral, &server).await {
                Ok(conn) => {
                    server.gate.client_key_ack.set(&server, &false).unwrap();
                    println!("Set authenticate_ack {}",false);
                    server.gate.authenticate_ack.set(&server, &false).unwrap();
                    server.gate.auth_action.set(&server, &1u16).unwrap(); // Default: open door
                    server.gate.management.set(&server, &0).unwrap();
                    // Populate management_name with current device name
                    let current_name = config.get_name("BlueGate").await;
                    let mut name_bytes = [0u8; 64];
                    let name_len = current_name.len().min(63);
                    name_bytes[..name_len].copy_from_slice(&current_name.as_bytes()[..name_len]);
                    server.gate.management_name.set(&server, &name_bytes).unwrap();
                    let mut nonce = [1 as u8; 32];
                    rng.fill_bytes(&mut nonce);
                    server.gate.nonce.set(&server, &nonce).unwrap();
                    // set up tasks when the connection is established to a central, so they don't run when no one is connected.
                    let timeout=config.get(ConfigSlot::ConnTimeout,2000).await;
                    let a = gatt_events_task(
                        &server,
                        &conn,
                        &mut keys,
                        &mut config,
                        &mut auth_log,
                        tx,
                        cfg_prog_mode,
                    );
                    // let b = custom_task(&conn, &stack);
                    let c = connection_timeout_task(&server, timeout);
                    // run until any task ends (usually because the connection has been closed),
                    // then return to advertising state.
                    select(a, c).await;
                    // select(select(a, b), c).await;
                }
                Err(e) => {
                    //#[cfg(feature = "defmt")]
                    //let e = defmt::Debug2Format(&e);
                    panic!("[adv] error: {:?}", e);
                }
            }
        }
    })
    .await;
}

/// This is a background task that is required to run forever alongside any other BLE tasks.
///
/// ## Alternative
///
/// If you didn't require this to be generic for your application, you could statically spawn this with i.e.
///
/// ```rust,ignore
///
/// #[embassy_executor::task]
/// async fn ble_task(mut runner: Runner<'static, SoftdeviceController<'static>>) {
///     runner.run().await;
/// }
///
/// spawner.must_spawn(ble_task(runner));
/// ```
async fn ble_task<C: Controller, P: PacketPool>(mut runner: Runner<'_, C, P>) {
    loop {
        if let Err(e) = runner.run().await {
            //#[cfg(feature = "defmt")]
            //let e = defmt::Debug2Format(&e);
            panic!("[ble_task] error: {:?}", e);
        }
    }
}

/// Stream Events until the connection closes.
///
/// This function will handle the GATT events and process them.
/// This is how we interact with read and write requests.
async fn gatt_events_task<P: PacketPool, S: NorFlash>(
    server: &Server<'_>,
    conn: &GattConnection<'_, '_, P>,
    keys: &mut KeyStore,
    config: &mut ConfigStore<S>,
    auth_log: &mut AuthLog,
    tx: Sender<'_, CriticalSectionRawMutex, FsmCommand, 4>,
    prog_mode: bool,
) -> Result<(), Error> {
    // let level = server.battery_service.level;
    let get_name = |handle| {
    if      handle == server.gate.nonce.handle { "nonce" }
    else if handle == server.gate.authenticate.handle { "authenticate" }
    else if handle == server.gate.client_pubkey.handle { "client_pubkey" }
    else if handle == server.gate.client_nonce.handle { "client_nonce" }
    else if handle == server.gate.client_key_ack.handle { "client_key_ack" }
    else if handle == server.gate.authenticate_ack.handle { "authenticate_ack" }
    else if handle == server.gate.perm.handle { "perm" }
    else if handle == server.gate.auth_action.handle { "auth_action" }
    else if handle == server.gate.management.handle { "management" }
    else if handle == server.gate.management_key.handle { "management_key" }
    else if handle == server.gate.management_param_id.handle { "management_param_id" }
    else if handle == server.gate.management_param_val.handle { "management_param_val" }
    else if handle == server.gate.management_name.handle { "management_name" }
    else if handle == server.gate.management_result.handle { "management_result" }
    else if handle == server.gate.log_index.handle { "log_index" }
    else {"unknown"}
    };
    let reason = loop {
        match conn.next().await {
            GattConnectionEvent::Disconnected { reason } => break reason,
            GattConnectionEvent::Gatt { event } => {
                match &event {
                    GattEvent::Read(event) => {
                        println!("Read {} ({})", get_name(event.handle()), event.handle());
                        if event.handle() == server.gate.log_count.handle {
                            let count = auth_log.count() as u16;
                            server.gate.log_count.set(server, &count).unwrap();
                        }
                        if event.handle() == server.gate.log_entry.handle {
                            let index = server.gate.log_index.get(server).unwrap_or(0) as usize;
                            let entry = auth_log.entry_bytes(index);
                            server.gate.log_entry.set(server, &entry).unwrap();
                        }
                        // if event.handle() == level.handle {
                        //     let value = server.get(&level);
                        //     println!("[gatt] Read Event to Level Characteristic: {:?}", value);
                        // }
                    }
                    GattEvent::Write(event) => {
                        println!("Write {} ({}) {}", get_name(event.handle()), event.handle(), HexFmt(event.data()));


                        if event.handle() == server.gate.authenticate.handle {
                            // println!(
                            //     "Authenticate {}, {}",
                            //     event.data().len(),
                            //     HexFmt(event.data())
                            // );
                            let mut auth_success = false;
                            let d = event.data();
                            if d.len() == 64 {
                                let mut hasher = Sha256::new();
                                hasher.update(&server.gate.nonce.get(&server).unwrap());
                                hasher.update(&server.gate.client_nonce.get(&server).unwrap());
                                let digest: [u8; 32] = hasher.finalize().into();
                                // println!("Digest {:?}", HexFmt(digest));
                                // println!("Signature {:?}", HexFmt(d));
                                let pubkey = server.gate.client_pubkey.get(server)?;
                                println!("PubKey {:?}", HexFmt(pubkey));

                                let keytype = pubkey[0];
                                if keytype == 1 {
                                    let key32: &[u8; 32] = pubkey[1..33].try_into().unwrap();
                                    let verifying_key = VerifyingKey::from_bytes(&key32)
                                        .map_err(|_| Error::InvalidValue)?;
                                        auth_success = verifying_key
                                        .verify(
                                            &digest,
                                            &ed25519::Signature::from_slice(&d[..]).unwrap(),
                                        )
                                        .is_ok();
                                } else if keytype == 2 || keytype == 3 {
                                    auth_success = verify_secp256r1_sha256(&digest, d, &pubkey)
                                }
                            }
                            let auth_action = server.gate.auth_action.get(server).unwrap_or(0);
                            let perm = server.gate.perm.get(server).unwrap_or(0);
                            println!("Auth {} perm {} action {}", auth_success, perm, auth_action);
                            server.gate.authenticate_ack.set(server, &auth_success).unwrap();
                            let pubkey = server.gate.client_pubkey.get(server).unwrap_or([0u8; 33]);
                            let mut addr_bytes = [0u8; 6];
                            addr_bytes.copy_from_slice(conn.raw().peer_address().raw());
                            auth_log.push(AuthLogEntry {
                                pubkey,
                                uptime_ms: Instant::now().as_millis(),
                                addr: addr_bytes,
                                success: auth_success,
                                auth_action,
                            });

                            if auth_success {
                                let action_code = auth_action & 0x7f;
                                match action_code {
                                    1 => {
                                        let r = tx.send(FsmCommand::Open).await;
                                        println!("Authenticated, opening door {:?}", r);
                                    }
                                    2 => {
                                        let r = tx.send(FsmCommand::Open).await;
                                        println!("Authenticated, opening door {:?}", r);
                                        if perm > 3 { //only available if user has any of flags
                                            let r = tx.send(FsmCommand::StopAutoClose).await;
                                            println!("Authenticated, stopping autoclose {:?}", r);
                                        }
                                    }
                                    3 => {
                                        let r = tx.send(FsmCommand::Close).await;
                                        println!("Authenticated, closing door {:?}", r);
                                    }
                                    _ => {}
                                }
                            }
                        }
                        if event.handle() == server.gate.client_pubkey.handle {
                            println!(
                                "Authorize {} bytes, {}",
                                event.data().len(),
                                HexFmt(event.data()),
                            );
                            let d = event.data();
                            let mut lookup_key = [0u8; 33];
                            let perm: u8;
                            if prog_mode {
                                perm=128;
                            }else{
                                if d.len() == 32 {
                                    // ed25519: flag byte 0x01, then 32 bytes of key
                                    lookup_key[0] = 0x01;
                                    lookup_key[1..].copy_from_slice(d);
                                    perm = keys.lookup(&lookup_key);
                                } else if d.len() == 33 {
                                    // secp256r1: first byte has flags, then 32 bytes
                                    lookup_key.copy_from_slice(d);
                                    perm = keys.lookup(&lookup_key);
                                } else {
                                    perm = 0;
                                }
                            }
                            let value = perm > 0;
                            println!("matched = {} perm {}", value, perm);
                            server.gate.client_key_ack.set(server, &value).unwrap();
                            server.gate.perm.set(server, &(perm & 0xfc)).unwrap();
                        }
                        if event.handle() == server.gate.log_index.handle {
                            if event.data().len() == 2 {
                                let index = u16::from_le_bytes([event.data()[0], event.data()[1]]);
                                server.gate.log_index.set(server, &index).unwrap();
                            }
                        }
                        // Management action handling (admin only)
                        if event.handle() == server.gate.management.handle {
                            let action = event.data().first().copied().unwrap_or(0);
                            let perm = server.gate.perm.get(server).unwrap_or(0);
                            println!("read authenticate_ack {:?}", server.gate.authenticate_ack.get(server));
                            let auth = server.gate.authenticate_ack.get(server).unwrap_or(false);
                            let is_admin = (perm & PERM_ADMIN) == PERM_ADMIN;
                            let is_admadmin = (perm & PERM_ADMADMIN) == PERM_ADMADMIN;
                            let is_setadmin = (perm & PERM_SETADMIN) == PERM_SETADMIN;

                            println!("Management action: 0x{:02x}, admin: {} auth {} perm {}", action, is_admin, auth, perm);

                            let result = if !is_admin || !auth {
                                MGMT_ERR_NOT_ADMIN
                            } else {
                                match action {
                                    MGMT_ADD_KEY => {
                                        let key = server.gate.management_key.get(server).unwrap_or([0; 33]);
                                        println!("Adding key: {}", HexFmt(&key));
                                        if (key[0] & 0xf0 == 0 ) | is_admadmin {
                                            match keys.add(config.flash(), key).await {
                                                Ok(true) => {
                                                    println!("Key added successfully");
                                                    MGMT_OK
                                                }
                                                Ok(false) => {
                                                    println!("Key already exists or store full");
                                                    MGMT_ERR_INVALID
                                                }
                                                Err(_) => {
                                                    println!("Flash error adding key");
                                                    MGMT_ERR_FLASH
                                                }
                                            }
                                        }else{
                                            MGMT_ERR_NOT_ADMIN
                                        }
                                    }
                                    MGMT_DEL_KEY => {
                                        let key = server.gate.management_key.get(server).unwrap_or([0; 33]);
                                        println!("Deleting key: {}", HexFmt(&key));
                                        let found = keys.lookup(&key);
                                        if found==0 {
                                            println!("Key not found");
                                            MGMT_ERR_NOT_FOUND
                                        }else if found & 0xf0 == 0 || is_admadmin {
                                            match keys.del(config.flash(), key).await {
                                                Ok(true) => {
                                                    println!("Key deleted successfully");
                                                    MGMT_OK
                                                }
                                                Ok(false) => {
                                                    println!("Key not found");
                                                    MGMT_ERR_NOT_FOUND
                                                }
                                                Err(_) => {
                                                    println!("Flash error deleting key");
                                                    MGMT_ERR_FLASH
                                                }
                                            }
                                        }else{
                                            MGMT_ERR_NOT_ADMIN
                                        }
                                    }
                                    MGMT_GET_KEY => {
                                        let index_bytes = server.gate.management_param_val.get(server).unwrap_or([0; 4]);
                                        let index = u32::from_le_bytes(index_bytes) as usize;
                                        let count = keys.len() as u32;
                                        println!("Getting key at index {} (total: {})", index, count);
                                        // Always set count in param_val
                                        server.gate.management_param_val.set(server, &count.to_le_bytes()).unwrap();
                                        match keys.get(index) {
                                            Some(key) => {
                                                println!("Key found: {}", HexFmt(key));
                                                server.gate.management_key.set(server, key).unwrap();
                                                MGMT_OK
                                            }
                                            None => {
                                                println!("Key index out of range");
                                                MGMT_ERR_NOT_FOUND
                                            }
                                        }
                                    }
                                    MGMT_SET_PARAM => {
                                        if is_setadmin {
                                            let slot = server.gate.management_param_id.get(server).unwrap_or(0);
                                            let value = u32::from_le_bytes(server.gate.management_param_val.get(server).unwrap_or([0,0,0,0]));
                                            println!("Setting param slot {} = {}", slot, value);
                                            if slot==31 {
                                                esp_hal::system::software_reset();
                                            }
                                            match config.set_slot(slot, value).await {
                                                Ok(()) => {
                                                    println!("Param set successfully");
                                                    MGMT_OK
                                                }
                                                Err(_) => {
                                                    println!("Flash error setting param");
                                                    MGMT_ERR_FLASH
                                                }
                                            }
                                        } else {
                                            MGMT_ERR_NOT_ADMIN
                                        }
                                    }
                                    MGMT_GET_PARAM => {
                                        let slot = server.gate.management_param_id.get(server).unwrap_or(0);
                                        let value = config.get_slot(slot, 0).await;
                                        let value_bytes = value.to_le_bytes();
                                        println!("Getting param slot {} = {} {:?}", slot, value, value_bytes);
                                        server.gate.management_param_val.set(server, &value_bytes).unwrap();
                                        MGMT_OK
                                    }
                                    MGMT_SET_NAME => {
                                        let name_bytes = server.gate.management_name.get(server).unwrap_or([0; 64]);
                                        let len = name_bytes.iter().position(|&b| b == 0).unwrap_or(64);
                                        let name_str = core::str::from_utf8(&name_bytes[..len]).unwrap_or("");
                                        println!("Setting name: {}", name_str);
                                        match config.set_name(name_str).await {
                                            Ok(()) => {
                                                println!("Name set successfully");
                                                MGMT_OK
                                            }
                                            Err(_) => {
                                                println!("Flash error setting name");
                                                MGMT_ERR_FLASH
                                            }
                                        }
                                    }
                                    _ => {
                                        println!("Unknown management action");
                                        MGMT_ERR_INVALID
                                    }
                                }
                            };

                            server.gate.management_result.set(server, &result).unwrap();
                        }
                    }
                    GattEvent::Other(_event) => {
                        // println!("other event {:?}", event.payload().handle());
                    }
                };
                // This step is also performed at drop(), but writing it explicitly is necessary
                // in order to ensure reply is sent.
                match event.accept() {
                    Ok(reply) => reply.send().await,
                    Err(e) => println!("[gatt] error sending response: {:?}", e),
                };
            }
            // GattConnectionEvent::PhyUpdated { .. } => {
            //     println!("GattConnectionEvent::PhyUpdated");
            // }
            // GattConnectionEvent::ConnectionParamsUpdated { .. } => {
            //     println!("GattConnectionEvent::ConnectionParamsUpdated");
            // }
            // GattConnectionEvent::RequestConnectionParams { .. } => {
            //     println!("GattConnectionEvent::RequestConnectionParams");
            // }
            // GattConnectionEvent::DataLengthUpdated {
            //     max_tx_octets,
            //     max_tx_time,
            //     max_rx_octets,
            //     max_rx_time,
            // } => {
            //     println!(
            //         "GattConnectionEvent::DataLengthUpdated {} {} {} {} ",
            //         max_tx_octets, max_tx_time, max_rx_octets, max_rx_time,
            //     );
            // }
            _any => {
                // println!("other GattConnectionEvent::?");
            } // ignore other Gatt Connection Events
        }
    };
    println!("[gatt] disconnected: {:?}", reason);
    Ok(())
}

/// Create an advertiser to use to connect to a BLE Central, and wait for it to connect.
async fn advertise<'values, 'server, C: Controller>(
    name: &'values str,
    peripheral: &mut Peripheral<'values, C, DefaultPacketPool>,
    server: &'server Server<'values>,
) -> Result<GattConnection<'values, 'server, DefaultPacketPool>, BleHostError<C::Error>> {
    // Gate service UUID: 6a7e6a7e-4929-42d0-0000-fcc5a35e13f1 (little-endian)
    const GATE_SERVICE_UUID: [u8; 16] = [
        0xf1, 0x13, 0x5e, 0xa3, 0xc5, 0xfc, 0x00, 0x00,
        0xd0, 0x42, 0x29, 0x49, 0x7e, 0x6a, 0x7e, 0x6a,
    ];
    // Advertising data: flags + 128-bit service UUID (uses ~20 bytes)
    let mut advertiser_data = [0; 31];
    let adv_len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::ServiceUuids128(&[GATE_SERVICE_UUID]),
        ],
        &mut advertiser_data[..],
    )?;
    // Scan response data: device name
    let mut scan_data = [0; 31];
    let scan_len = AdStructure::encode_slice(
        &[
            AdStructure::CompleteLocalName(name.as_bytes()),
        ],
        &mut scan_data[..],
    )?;
    let advertiser = peripheral
        .advertise(
            &Default::default(),
            Advertisement::ConnectableScannableUndirected {
                adv_data: &advertiser_data[..adv_len],
                scan_data: &scan_data[..scan_len],
            },
        )
        .await?;
    println!("[adv] advertising");
    let conn = advertiser.accept().await?.with_attribute_server(server)?;
    println!("[adv] connection established with {:?}",conn.raw().peer_address());
    Ok(conn)
}

/// Connection timeout task.
/// Disconnects the client after 1 second unless they are an authenticated admin in admin mode.
async fn connection_timeout_task(server: &Server<'_>, duration: u32) {
    Timer::after_millis(duration.into()).await;

    let is_authenticated = server.gate.authenticate_ack.get(server).unwrap_or(false);
    let perm = server.gate.perm.get(server).unwrap_or(0);
    let is_admin = (perm & PERM_ADMIN) == PERM_ADMIN;
    let auth_action = server.gate.auth_action.get(server).unwrap_or(0);
    let is_admin_mode = (auth_action & 128) != 0;

    if is_authenticated && is_admin && is_admin_mode {
        // Admin is connected in admin mode, keep connection alive indefinitely
        println!("[timeout] Admin authenticated in admin mode, keeping connection");
        loop {
            Timer::after_secs(3600).await;
        }
    } else {
        // Not an authenticated admin in admin mode, disconnect
        println!("[timeout] Connection timeout, disconnecting (auth={}, admin={}, admin_mode={})", is_authenticated, is_admin, is_admin_mode);
        // Returning from this task will cause the select to complete and disconnect
    }
}

/// Example task to use the BLE notifier interface.
/// This task will notify the connected central of a counter value every 2 seconds.
/// It will also read the RSSI value every 2 seconds.
/// and will stop when the connection is closed by the central or an error occurs.
/*
async fn custom_task<C: Controller, P: PacketPool>(
    conn: &GattConnection<'_, '_, P>,
    stack: &Stack<'_, C, P>,
) {
    let mut tick: u8 = 0;
    // let level = server.battery_service.level;
    loop {
        tick = tick.wrapping_add(1);
        // if tick > 2 {
        //     println!("stopping");
        //     break;
        // }
        // println!("[custom_task] notifying connection of tick {}", tick);
        // if level.notify(conn, &tick).await.is_err() {
        //     println!("[custom_task] error notifying connection");
        //     break;
        // };
        // read RSSI (Received Signal Strength Indicator) of the connection.
        if let Ok(rssi) = conn.raw().rssi(stack).await {
            println!("[custom_task] RSSI: {:?}", rssi);
        } else {
            println!("[custom_task] error getting RSSI");
            break;
        };

        // let v = server.battery_service.level.get(&server).unwrap();
        // if v < 99 {
        //     let v1 = v + 1;
        //     server.battery_service.level.set(&server, &v1).unwrap();
        // } else {
        //     server.battery_service.level.set(&server, &1).unwrap();
        // }

        Timer::after_secs(2).await;
    }
}
*/

pub fn verify_secp256r1_sha256(hash: &[u8; 32], sig: &[u8], pk: &[u8; 33]) -> bool {
    // 1) Parse the compressed SEC1 public key (33 bytes, 0x02/0x03 + X)
    let verifying_key = match p256::ecdsa::VerifyingKey::from_sec1_bytes(pk) {
        Ok(vk) => vk,
        Err(_) => return false, // invalid public key encoding
    };

    // 2) Parse the 64-byte raw (r || s) signature
    let signature = match p256::ecdsa::Signature::from_slice(sig) {
        Ok(s) => s,
        Err(_) => return false, // invalid signature encoding
    };

    // 3) Verify prehashed message (we already have SHA-256(hash))
    verifying_key.verify_prehash(hash, &signature).is_ok()
}
