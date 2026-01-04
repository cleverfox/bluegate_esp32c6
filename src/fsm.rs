//! FSM (Finite State Machine) module
//!
//! Controls the gate opening/closing sequence, interacts with GPI and GPO modules.

use embassy_futures::select::{select, select3, Either, Either3};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel, signal::Signal,
};
use embassy_time::{Instant, Duration, Timer};

use crate::gpi::GPI_CHANNEL;
use crate::gpo::commands;
use crate::types::{Door, DoorConfig, FsmCommand, GateConfig, GateState, GpiEvent};

use esp_println::println;
/// Channel for sending commands to the FSM from external processes
pub static FSM_COMMAND_CHANNEL: Channel<CriticalSectionRawMutex, FsmCommand, 4> = Channel::new();

/// Signal to abort closing operation (used for obstacle detection)
static ABORT_CLOSE_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Current gate state (for external monitoring if needed)
static CURRENT_STATE: Signal<CriticalSectionRawMutex, GateState> = Signal::new();

/// Helper to send a command to the FSM
pub async fn send_fsm_command(cmd: FsmCommand) {
    println!("FSM Command {:?}",cmd);
    FSM_COMMAND_CHANNEL.send(cmd).await;
}

/// Get the current gate state
pub fn get_state() -> GateState {
    // Default to Closed if not yet signaled
    CURRENT_STATE.try_take().unwrap_or(GateState::Closed)
}

fn set_state(state: GateState) {
    println!("FSM state {:?} -> {:?}", get_state(), state);
    CURRENT_STATE.signal(state);
}

/// FSM task - main state machine for gate control
#[embassy_executor::task]
pub async fn fsm_task(config: GateConfig) {
    println!("FSM task started");
    set_state(GateState::Closed);

    // Ensure all outputs are off at start
    commands::stop_all_doors().await;
    commands::lamp_off().await;

    loop {
        match get_state() {
            GateState::Closed => {
                handle_closed_state(&config).await;
            }
            GateState::Opening => {
                handle_opening_state(&config).await;
            }
            GateState::Open => {
                handle_open_state(&config).await;
            }
            GateState::Closing => {
                handle_closing_state(&config).await;
            }
        }
    }
}

/// Handle the Closed state - wait for Open command or control pulse
async fn handle_closed_state(_config: &GateConfig) {
    println!("Waiting for open command...");

    loop {
        // Wait for either FSM command or GPI event
        match select(FSM_COMMAND_CHANNEL.receive(), GPI_CHANNEL.receive()).await {
            Either::First(cmd) => match cmd {
                FsmCommand::Open => {
                    set_state(GateState::Opening);
                    return;
                }
                FsmCommand::Close => {
                    // Already closed, ignore
                    println!("Already closed, ignoring Close command");
                }
                FsmCommand::StopAutoClose => {
                    // Not relevant in closed state
                }
            },
            Either::Second(event) => match event {
                GpiEvent::ControlPulse => {
                    set_state(GateState::Opening);
                    return;
                }
                GpiEvent::ObstacleDetected | GpiEvent::ObstacleCleared => {
                    // Ignore obstacle events in closed state
                }
            },
        }
    }
}

/// Handle the Opening state - open both doors in parallel
async fn handle_opening_state(config: &GateConfig) {
    println!("Starting opening sequence");

    // Start lamp blinking (1 second before door movement)
    commands::lamp_on().await;
    Timer::after(config.lamp_prestart).await;

    // Open both doors in parallel using join
    let left_future = open_door(Door::Left, &config.left_door);
    let right_future = open_door(Door::Right, &config.right_door);

    // Use embassy_futures::join to run both in parallel
    embassy_futures::join::join(left_future, right_future).await;

    // Stop lamp after doors are fully open
    commands::lamp_off().await;

    set_state(GateState::Open);
}

/// Open a single door with its timing configuration
async fn open_door(door: Door, config: &DoorConfig) {
    println!("Opening {:?} door", door);

    // Wait for open delay
    if config.open_delay.as_millis() > 0 {
        Timer::after(config.open_delay).await;
    }

    // Start opening
    commands::start_opening(door).await;

    // Wait for open duration
    Timer::after(config.open_duration).await;

    // Stop opening
    commands::stop_opening(door).await;
    println!("{:?} door fully open", door);
}

/// Handle the Open state - wait for close command or autoclose timeout
async fn handle_open_state(config: &GateConfig) {
    println!("Doors fully open");

    let mut autoclose_enabled = config.autoclose_delay.is_some();
    let mut do_close : u8 = 0;
    let mut last_close_attempt = Instant::now().as_millis();

    loop {
        if autoclose_enabled {
            if let Some(delay) = config.autoclose_delay {
                println!("Autoclose enabled, waiting {} seconds", delay.as_secs());

                // Wait for autoclose timeout, command, or GPI event
                match select3(
                    Timer::after(delay),
                    FSM_COMMAND_CHANNEL.receive(),
                    GPI_CHANNEL.receive(),
                )
                .await
                {
                    Either3::First(_) => {
                        // Autoclose timeout expired
                        println!("Autoclose timeout, starting close sequence");
                        set_state(GateState::Closing);
                        return;
                    }
                    Either3::Second(cmd) => match cmd {
                        FsmCommand::Open => {
                            // Already open, reset autoclose timer
                            println!("Open command while open, resetting autoclose timer");
                            continue;
                        }
                        FsmCommand::Close => {
                            set_state(GateState::Closing);
                            return;
                        }
                        FsmCommand::StopAutoClose => {
                            println!("Autoclose disabled");
                            autoclose_enabled = false;
                            continue;
                        }
                    },
                    Either3::Third(event) => match event {
                        GpiEvent::ControlPulse => {
                            println!("Control pulse while open, resetting autoclose timer");
                            // Control pulse while open = close
                            // set_state(GateState::Closing);
                            continue;
                        }
                        GpiEvent::ObstacleDetected => {
                            println!("Obstacle event while open, resetting autoclose timer");
                            // Ignore obstacle events in open state
                            continue;
                        }
                        GpiEvent::ObstacleCleared => {
                            println!("Obstacle cleared, resetting autoclose timer");
                            continue;
                        }
                    },
                }
            }
        } else {
            // Autoclose disabled, wait for explicit close command
            match select(FSM_COMMAND_CHANNEL.receive(), GPI_CHANNEL.receive()).await {
                Either::First(cmd) => match cmd {
                    FsmCommand::Open => {
                        // Already open, ignore
                    }
                    FsmCommand::Close => {
                        set_state(GateState::Closing);
                        return;
                    }
                    FsmCommand::StopAutoClose => {
                        // Already disabled
                    }
                },
                Either::Second(event) => match event {
                    GpiEvent::ControlPulse => {
                        // Control pulse while open = close
                        // call 3 times with less than 10 sec interval to close
                        let now = Instant::now().as_millis();
                        if (now-last_close_attempt) < 10000 {
                            do_close += 1;
                        }else{
                            do_close = 1;
                        }
                        last_close_attempt = now;
                        println!("Autoclose disabled, control pulse {} of 3",do_close);
                        if do_close > 2 {
                            set_state(GateState::Closing);
                            return;
                        }
                    }
                    GpiEvent::ObstacleDetected | GpiEvent::ObstacleCleared => {
                        // Ignore obstacle events in open state
                    }
                },
            }
        }
    }
}

/// Handle the Closing state - close both doors in parallel, monitor for obstacles
async fn handle_closing_state(config: &GateConfig) {
    println!("Starting closing sequence");

    // Start lamp blinking (1 second before door movement)
    commands::lamp_on().await;
    Timer::after(config.lamp_prestart).await;

    // Reset abort signal
    ABORT_CLOSE_SIGNAL.reset();

    // Close both doors in parallel, monitoring for obstacles
    let left_future = close_door_with_obstacle_monitor(Door::Left, &config.left_door);
    let right_future = close_door_with_obstacle_monitor(Door::Right, &config.right_door);
    let obstacle_monitor = obstacle_monitor_task();

    // Run closing and obstacle monitoring in parallel
    let result = select(
        embassy_futures::join::join(left_future, right_future),
        obstacle_monitor,
    )
    .await;

    match result {
        Either::First(_) => {
            // Doors closed successfully
            commands::lamp_off().await;
            set_state(GateState::Closed);
        }
        Either::Second(_) => {
            // Obstacle detected during close
            println!("Obstacle detected during close, reversing!");

            // Stop closing immediately
            commands::stop_closing(Door::Left).await;
            commands::stop_closing(Door::Right).await;

            // Signal abort to any waiting close operations
            ABORT_CLOSE_SIGNAL.signal(());

            // Small delay before reversing
            Timer::after(Duration::from_millis(100)).await;

            // Transition to opening state (reverse)
            set_state(GateState::Opening);
        }
    }
}

/// Close a single door with its timing configuration
/// This task can be aborted by the obstacle detection
async fn close_door_with_obstacle_monitor(door: Door, config: &DoorConfig) {
    println!("Closing {:?} door", door);

    // Wait for close delay (check for abort during this time too)
    if config.close_delay.as_millis() > 0 {
        match select(Timer::after(config.close_delay), ABORT_CLOSE_SIGNAL.wait()).await {
            Either::First(_) => {}
            Either::Second(_) => {
                println!("{:?} door close aborted during delay", door);
                return;
            }
        }
    }

    // Start closing
    commands::start_closing(door).await;

    // Wait for close duration or abort
    match select(
        Timer::after(config.close_duration),
        ABORT_CLOSE_SIGNAL.wait(),
    )
    .await
    {
        Either::First(_) => {
            // Normal completion
            commands::stop_closing(door).await;
            println!("{:?} door fully closed", door);
        }
        Either::Second(_) => {
            // Aborted
            commands::stop_closing(door).await;
            println!("{:?} door close aborted", door);
        }
    }
}

/// Monitor for obstacles during closing sequence
/// Returns when an obstacle is detected
async fn obstacle_monitor_task() {
    loop {
        // Also check for commands that might come in
        match select(GPI_CHANNEL.receive(), FSM_COMMAND_CHANNEL.receive()).await {
            Either::First(event) => match event {
                GpiEvent::ObstacleDetected => {
                    println!("Obstacle monitor: obstacle detected!");
                    return;
                }
                GpiEvent::ControlPulse | GpiEvent::ObstacleCleared => {
                    // Ignore other events during close monitoring
                }
            },
            Either::Second(cmd) => {
                // Could handle emergency commands here if needed
                match cmd {
                    FsmCommand::Open => {
                        // Treat open command as obstacle - reverse
                        println!("Open command during close - reversing");
                        return;
                    }
                    FsmCommand::Close | FsmCommand::StopAutoClose => {
                        // Ignore
                    }
                }
            }
        }
    }
}
