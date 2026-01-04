//! GPI (General Purpose Input) module
//!
//! Monitors input pins with debouncing and generates events for the FSM.

use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::gpio::{Input };

use crate::types::GpiEvent;
use esp_println::println;

/// Channel for sending events from GPI to FSM
pub static GPI_CHANNEL: Channel<CriticalSectionRawMutex, GpiEvent, 8> = Channel::new();

/// Debounce configuration
const DEBOUNCE_TIME: Duration = Duration::from_millis(100);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const OBSTACLE_REPEAT: Duration = Duration::from_secs(3);

/// Debouncer state for a single input
struct Debouncer {
    stable_state: bool,
    last_change: Instant,
    pending_state: bool,
}

impl Debouncer {
    fn new(initial_state: bool) -> Self {
        Self {
            stable_state: initial_state,
            last_change: Instant::now(),
            pending_state: initial_state,
        }
    }

    /// Update the debouncer with a new raw reading
    /// Returns Some(true) if rising edge detected, Some(false) if falling edge
    fn update(&mut self, raw_state: bool) -> Option<bool> {
        let now = Instant::now();

        if raw_state != self.pending_state {
            // State changed, reset timer
            self.pending_state = raw_state;
            self.last_change = now;
            None
        } else if raw_state != self.stable_state {
            // State is different from stable but matches pending
            if now.duration_since(self.last_change) >= DEBOUNCE_TIME {
                // Debounce time passed, accept new state
                self.stable_state = raw_state;
                Some(raw_state)
            } else {
                None
            }
        } else {
            None
        }
    }

    // fn is_active(&self) -> bool {
    //     self.stable_state
    // }
}

/// GPI task - monitors control and obstacle inputs
///
/// Generates events when debounced state changes occur.
/// Control input: generates ControlPulse on rising edge
/// Obstacle input: generates ObstacleDetected/ObstacleCleared on state changes
#[embassy_executor::task]
pub async fn gpi_task(control_pin: Input<'static>, obstacle_pin: Input<'static>, polarity: u32) {
    println!("GPI task started polarity {}",polarity & 255);

    // Initialize debouncers with current pin states
    // Assuming active high for both inputs (adjust as needed)
    let mut control_debouncer = Debouncer::new(if polarity&1==0 {control_pin.is_low()}else{control_pin.is_high()});
    let mut obstacle_debouncer = Debouncer::new(if polarity&2==0 {obstacle_pin.is_low()}else{obstacle_pin.is_high()});
    let mut last_obstacle_report: Option<Instant> = None;

    loop {
        Timer::after(POLL_INTERVAL).await;

        // Read current states (active high - adjust polarity as needed)
        let control_raw = if polarity&1==0 {control_pin.is_low()}else{control_pin.is_high()};
        let obstacle_raw = if polarity&2==0 {obstacle_pin.is_low()}else{obstacle_pin.is_high()};

        // Update control input debouncer
        if let Some(edge) = control_debouncer.update(control_raw) {
            if edge {
                // Rising edge on control input = pulse detected
                println!("GPI: Control pulse detected");
                GPI_CHANNEL.send(GpiEvent::ControlPulse).await;
            }
            // Falling edge is ignored for control input
        }

        // Update obstacle input debouncer
        if let Some(edge) = obstacle_debouncer.update(obstacle_raw) {
            if edge {
                println!("GPI: Obstacle detected");
                GPI_CHANNEL.send(GpiEvent::ObstacleDetected).await;
                last_obstacle_report = Some(Instant::now());
            } else {
                println!("GPI: Obstacle cleared");
                GPI_CHANNEL.send(GpiEvent::ObstacleCleared).await;
                last_obstacle_report = None;
            }
        }

        if obstacle_raw {
            let now = Instant::now();
            let should_report = match last_obstacle_report {
                None => true,
                Some(last) => now.duration_since(last) >= OBSTACLE_REPEAT,
            };

            if should_report {
                println!("GPI: Obstacle detected");
                GPI_CHANNEL.send(GpiEvent::ObstacleDetected).await;
                last_obstacle_report = Some(now);
            }
        } else {
            last_obstacle_report = None;
        }
    }
}

/// Check if obstacle is currently detected (for synchronous checking during close sequence)
/// Note: This is a snapshot and should be used with appropriate synchronization
pub fn is_obstacle_active() -> bool {
    // This would need to be implemented with a shared state if synchronous access is needed
    // For the async approach, we rely on events through the channel
    false
}
