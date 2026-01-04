# Two-Door Gate Controller

An async gate controller for ESP32 using the Embassy framework, written in Rust.

## Features

- **Two independent doors** with configurable timing (open/close delays and durations)
- **Signal lamp** with 1/2 Hz blinking (75% duty cycle) during door movement
- **Obstacle detection** with automatic reversal during closing
- **Autoclose** with configurable delay (can be disabled)
- **Debounced inputs** with 500ms minimum pulse width
- **Fully async** architecture using Embassy

## Architecture

The controller is split into three async tasks communicating via channels:

```
┌─────────────────────────────────────────────────────────────┐
│                         FSM Task                             │
│  (State machine: Closed → Opening → Open → Closing → ...)   │
└─────────────────────────────────────────────────────────────┘
        ▲                                        │
        │ GpiEvent                               │ GpoCommand
        │ (ControlPulse,                         │ (SetDoorOpen,
        │  ObstacleDetected,                     │  SetDoorClose,
        │  ObstacleCleared)                      │  SetLamp)
        │                                        ▼
┌───────────────────┐                   ┌───────────────────┐
│     GPI Task      │                   │     GPO Task      │
│ (Input monitoring │                   │ (Output control   │
│  with debouncing) │                   │  with lamp logic) │
└───────────────────┘                   └───────────────────┘
        ▲                                        │
        │                                        ▼
   ┌─────────┐                            ┌─────────────┐
   │ Control │                            │ Left Open   │
   │  Input  │                            │ Left Close  │
   │         │                            │ Right Open  │
   │Obstacle │                            │ Right Close │
   │  Input  │                            │ Signal Lamp │
   └─────────┘                            └─────────────┘
```

### Tasks

1. **GPO Task** (`gpo.rs`): Controls output pins via a command channel
   - Manages door open/close relays
   - Handles lamp blinking logic (1.5s on, 0.5s off = 1/2 Hz, 75% duty)

2. **GPI Task** (`gpi.rs`): Monitors inputs and generates events
   - Control input: Rising edge generates `ControlPulse` event
   - Obstacle input: State changes generate `ObstacleDetected`/`ObstacleCleared` events
   - 500ms debounce on all inputs

3. **FSM Task** (`fsm.rs`): Main state machine
   - States: `Closed`, `Opening`, `Open`, `Closing`
   - Receives commands via channel (`Open`, `Close`, `StopAutoClose`)
   - Coordinates door movements with configurable timing
   - Handles obstacle detection during closing (reverses to opening)

## Pin Assignments

| Pin | Function | Direction | Notes |
|-----|----------|-----------|-------|
| GPIO25 | Left Door Open | Output | Active high |
| GPIO26 | Left Door Close | Output | Active high |
| GPIO27 | Right Door Open | Output | Active high |
| GPIO14 | Right Door Close | Output | Active high |
| GPIO12 | Signal Lamp | Output | Active high |
| GPIO32 | Control Input | Input | Pull-down, active high |
| GPIO33 | Obstacle Input | Input | Pull-down, active high |

Adjust polarities in the code as needed for your hardware.

## Configuration

Edit the `GateConfig` in `main.rs`:

```rust
let config = GateConfig {
    left_door: DoorConfig::new(
        Duration::from_millis(0),      // open_delay
        Duration::from_millis(0),      // close_delay
        Duration::from_secs(15),       // open_duration
        Duration::from_secs(15),       // close_duration
    ),
    right_door: DoorConfig::new(
        Duration::from_millis(500),    // open_delay (500ms after left)
        Duration::from_millis(500),    // close_delay
        Duration::from_secs(15),       // open_duration
        Duration::from_secs(15),       // close_duration
    ),
    autoclose_delay: Some(Duration::from_secs(30)), // Set to None to disable
    lamp_prestart: Duration::from_secs(1),          // Lamp starts before movement
};
```

## Operation Sequence

### Opening Sequence

1. `Open` command received (via channel or control input pulse)
2. State → `Opening`
3. Signal lamp starts blinking
4. Wait `lamp_prestart` (1 second)
5. For each door in parallel:
   - Wait `open_delay`
   - Activate open relay
   - Wait `open_duration`
   - Deactivate open relay
6. Signal lamp stops
7. State → `Open`

### Open State

- If autoclose enabled: Wait `autoclose_delay`, then start closing
- `StopAutoClose` command temporarily disables autoclose
- `Close` command or control pulse starts closing
- `Open` command resets the autoclose timer

### Closing Sequence

1. State → `Closing`
2. Signal lamp starts blinking
3. Wait `lamp_prestart` (1 second)
4. For each door in parallel:
   - Wait `close_delay`
   - Activate close relay
   - Wait `close_duration`
   - Deactivate close relay
5. **During closing**, if obstacle detected:
   - Immediately stop all close relays
   - State → `Opening` (reverse)
6. If completed normally:
   - Signal lamp stops
   - State → `Closed`

## Building

```bash
# Install the Xtensa Rust toolchain
espup install

# Build the project
cargo build --release

# Flash to ESP32
cargo run --release
```

## External Control

Other tasks can send commands to the FSM:

```rust
use gate_controller::fsm::send_fsm_command;
use gate_controller::types::FsmCommand;

// Open the gate
send_fsm_command(FsmCommand::Open).await;

// Close the gate
send_fsm_command(FsmCommand::Close).await;

// Disable autoclose
send_fsm_command(FsmCommand::StopAutoClose).await;
```

## License

MIT
