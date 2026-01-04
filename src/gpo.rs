//! GPO (General Purpose Output) module
//!
//! Controls the output pins through a channel-based interface.
//! Handles the signal lamp blinking logic internally.

use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use embassy_time::{Duration};
use esp_hal::gpio::{Level, Output};

use crate::types::{Door, GpoCommand, LampState};
use esp_println::println;

/// Channel for sending commands to the GPO task
pub static GPO_CHANNEL: Channel<CriticalSectionRawMutex, GpoCommand, 8> = Channel::new();

/// Internal state for the GPO module
struct GpoState {
    left_open: bool,
    left_close: bool,
    right_open: bool,
    right_close: bool,
    lamp_state: LampState,
}

impl GpoState {
    const fn new() -> Self {
        Self {
            left_open: false,
            left_close: false,
            right_open: false,
            right_close: false,
            lamp_state: LampState::Off,
        }
    }

    fn apply_command(&mut self, cmd: GpoCommand) {
        match cmd {
            GpoCommand::SetDoorOpen { door, active } => match door {
                Door::Left => self.left_open = active,
                Door::Right => self.right_open = active,
            },
            GpoCommand::SetDoorClose { door, active } => match door {
                Door::Left => self.left_close = active,
                Door::Right => self.right_close = active,
            },
            GpoCommand::SetLamp(state) => self.lamp_state = state,
        }
    }
}

/// GPO task - controls all output pins
///
/// Receives commands through GPO_CHANNEL and updates output pins accordingly.
/// The lamp blinking is handled with a timer-based approach.
#[embassy_executor::task]
pub async fn gpo_task(
    mut lamp_pin: Output<'static>,
    mut left_open_pin: Output<'static>,
    mut left_close_pin: Output<'static>,
    mut right_open_pin: Output<'static>,
    mut right_close_pin: Output<'static>,
    polarity: u32,
) {
    println!("GPO task started polarity {}",polarity & 255);

    let mut state = GpoState::new();
    // let mut lamp_on = false;

    // Lamp timing: 1/2 Hz = 2 second period, 75% duty = 1.5s on, 0.5s off
    // const LAMP_ON_TIME: Duration = Duration::from_millis(1500);
    // const LAMP_OFF_TIME: Duration = Duration::from_millis(500);
    const POLL_INTERVAL: Duration = Duration::from_millis(50);

    // let mut lamp_timer_ms: u64 = 0;

    loop {
        // Try to receive a command (non-blocking with timeout)
        match embassy_time::with_timeout(POLL_INTERVAL, GPO_CHANNEL.receive()).await {
            Ok(cmd) => {
                println!("GPO received command: {:?}", cmd);
                state.apply_command(cmd);
            }
            Err(_) => {
                // Timeout - just continue with lamp logic
            }
        }

        // Update door outputs (active high - adjust polarity as needed)
        left_open_pin.set_level(if state.left_open {
            Level::High
        } else {
            Level::Low
        });
        left_close_pin.set_level(if state.left_close {
            Level::High
        } else {
            Level::Low
        });
        right_open_pin.set_level(if state.right_open {
            Level::High
        } else {
            Level::Low
        });
        right_close_pin.set_level(if state.right_close {
            Level::High
        } else {
            Level::Low
        });

        // Handle lamp blinking
        match state.lamp_state {
            LampState::Off => {
                if polarity&1 != 0 {
                    lamp_pin.set_high();
                }else{
                    lamp_pin.set_low();
                }
                // lamp_on = false;
                // lamp_timer_ms = 0;
            }
            LampState::Blinking => {
                if polarity&1 == 0 {
                    lamp_pin.set_high();
                }else{
                    lamp_pin.set_low();
                }
                // lamp_timer_ms += POLL_INTERVAL.as_millis();

                // if lamp_on {
                //     if lamp_timer_ms >= LAMP_ON_TIME.as_millis() {
                //         lamp_on = false;
                //         lamp_timer_ms = 0;
                //     }
                // } else {
                //     if lamp_timer_ms >= LAMP_OFF_TIME.as_millis() {
                //         lamp_on = true;
                //         lamp_timer_ms = 0;
                //     }
                // }

                // lamp_pin.set_level(if lamp_on^(polarity&1!=0) { Level::High } else { Level::Low });
            }
        }
    }
}

/// Helper to send a command to the GPO task
pub async fn send_gpo_command(cmd: GpoCommand) {
    GPO_CHANNEL.send(cmd).await;
}

/// Convenience functions for common operations
pub mod commands {
    use super::*;

    pub async fn start_opening(door: Door) {
        send_gpo_command(GpoCommand::SetDoorOpen { door, active: true }).await;
    }

    pub async fn stop_opening(door: Door) {
        send_gpo_command(GpoCommand::SetDoorOpen {
            door,
            active: false,
        })
        .await;
    }

    pub async fn start_closing(door: Door) {
        send_gpo_command(GpoCommand::SetDoorClose { door, active: true }).await;
    }

    pub async fn stop_closing(door: Door) {
        send_gpo_command(GpoCommand::SetDoorClose {
            door,
            active: false,
        })
        .await;
    }

    pub async fn lamp_on() {
        send_gpo_command(GpoCommand::SetLamp(LampState::Blinking)).await;
    }

    pub async fn lamp_off() {
        send_gpo_command(GpoCommand::SetLamp(LampState::Off)).await;
    }

    /// Stop all door movements
    pub async fn stop_all_doors() {
        send_gpo_command(GpoCommand::SetDoorOpen {
            door: Door::Left,
            active: false,
        })
        .await;
        send_gpo_command(GpoCommand::SetDoorOpen {
            door: Door::Right,
            active: false,
        })
        .await;
        send_gpo_command(GpoCommand::SetDoorClose {
            door: Door::Left,
            active: false,
        })
        .await;
        send_gpo_command(GpoCommand::SetDoorClose {
            door: Door::Right,
            active: false,
        })
        .await;
    }
}
