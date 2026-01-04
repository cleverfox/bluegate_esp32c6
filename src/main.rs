#![no_std]
#![no_main]

use blue_gate::ble_bas_peripheral;
use blue_gate::fsm::{fsm_task, FSM_COMMAND_CHANNEL};
use blue_gate::gpi::gpi_task;
use embassy_time::{Duration};
use blue_gate::gpo::gpo_task;
use blue_gate::keys::KeyStore;
use blue_gate::settings::{ConfigStore, ConfigSlot};
use blue_gate::types::GateConfig;
use embassy_executor::Spawner;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    rng::{Trng, TrngSource},
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_radio::ble::controller::BleConnector;
use esp_storage::FlashStorage;
use trouble_host::prelude::ExternalController;

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger_from_env();
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    esp_alloc::heap_allocator!(size: 72 * 1024);

    // Output pins for GPO task
    let lamp = Output::new(peripherals.GPIO7,   Level::Low, OutputConfig::default());
    let lclose = Output::new(peripherals.GPIO1, Level::Low, OutputConfig::default());
    let lopen = Output::new(peripherals.GPIO2,  Level::Low, OutputConfig::default());
    let rclose = Output::new(peripherals.GPIO4, Level::Low, OutputConfig::default());
    let ropen = Output::new(peripherals.GPIO3,  Level::Low, OutputConfig::default());

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    #[cfg(target_arch = "riscv32")]
    let software_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);

    esp_rtos::start(
        timg0.timer0,
        #[cfg(target_arch = "riscv32")]
        software_interrupt.software_interrupt0,
    );

    let radio = esp_radio::init().unwrap();
    let bluetooth = peripherals.BT;
    let connector = BleConnector::new(&radio, bluetooth, Default::default()).unwrap();
    let controller: ExternalController<_, 20> = ExternalController::new(connector);

    let _trng_source = TrngSource::new(peripherals.RNG, peripherals.ADC1);
    let mut trng = Trng::try_new().unwrap();
    let flash =
        embassy_embedded_hal::adapter::BlockingAsync::new(FlashStorage::new(peripherals.FLASH));

    // Initialize stores from flash (keys takes ownership, config created after keys loads data)
    let (keys, flash) = KeyStore::new(flash).await;
    println!("Loaded {} keys from flash", keys.len());
    let mut config = ConfigStore::new(flash).await;
    let polarity: u32 = config.get(ConfigSlot::IOPolarity,0).await;
    println!("Polarity mask {}",polarity);

    // Input pins for GPI task
    let trigger = Input::new(
        peripherals.GPIO15,
        InputConfig::default().with_pull(Pull::Down),
    );

    let obstacle = Input::new(
        peripherals.GPIO14,
        InputConfig::default().with_pull(Pull::Down),
    );

    let prog_mode = Input::new(
        peripherals.GPIO20,
        InputConfig::default().with_pull(Pull::Up),
    );

    // Spawn GPI task (monitors trigger and obstacle inputs)
    spawner.spawn(gpi_task(trigger, obstacle, polarity >> 8)).unwrap();

    // Spawn GPO task (controls door relays and lamp)
    spawner
        .spawn(gpo_task(lamp, lopen, lclose, ropen, rclose, polarity))
        .unwrap();

    // Spawn FSM task with default gate configuration
    let gate_config = GateConfig{
                left_door: blue_gate::types::DoorConfig::new(
                    Duration::from_millis(config.get(ConfigSlot::LeftOpenDelay,100).await.into()),
                    Duration::from_millis(config.get(ConfigSlot::LeftCloseDelay,800).await.into()),
                    Duration::from_millis(config.get(ConfigSlot::LeftOpenDuration,2000).await.into()),
                    Duration::from_millis(config.get(ConfigSlot::LeftCloseDuration,2000).await.into()),
                ),
                right_door: blue_gate::types::DoorConfig::new(
                    Duration::from_millis(config.get(ConfigSlot::RightOpenDelay,800).await.into()),
                    Duration::from_millis(config.get(ConfigSlot::RightCloseDelay,100).await.into()),
                    Duration::from_millis(config.get(ConfigSlot::RightOpenDuration,2000).await.into()),
                    Duration::from_millis(config.get(ConfigSlot::RightCloseDuration,2000).await.into()),
                ),
                autoclose_delay: match config.get(ConfigSlot::AutoClose,5000).await {
                    0 => None,
                    n => Some(Duration::from_millis(n.into()))
                },
                lamp_prestart:  Duration::from_millis(config.get(ConfigSlot::LampPreStart,500).await.into()),
            }; //GateConfig::default();

    spawner.spawn(fsm_task(gate_config)).unwrap();

    // Get sender for FSM commands from BLE
    let cmdtx = FSM_COMMAND_CHANNEL.sender();

    // Initialize config store from flash
    let device_name = config.get_name("BlueGate").await;
    println!("Device name: {}", device_name.as_str());

    // Run BLE peripheral
    ble_bas_peripheral::run(controller, &mut trng, &device_name, keys, config, cmdtx, prog_mode.is_low()).await;
}
