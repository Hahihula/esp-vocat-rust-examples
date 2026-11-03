# ESP-VoCat Rust examples

Bare-metal (`no_std`) Rust demos for the
[ESP-VoCat v1.0](https://docs.espressif.com/projects/esp-dev-kits/en/latest/esp32s3/esp-vocat/user_guide_v1.0.html)
AI development kit (ESP32-S3, 1.85" round QSPI touch LCD, dual microphones, speaker,
IMU, fuel gauge, microSD), built on [esp-hal](https://github.com/esp-rs/esp-hal) and
Embassy.

The pin map in `src/board.rs` follows the
[v1.0 schematic](https://dl.espressif.com/AE/esp-dev-kits/ESP-VoCat_SCH_V1_0.pdf).
ESP-VoCat v1.2 moved several pins (UART1, I2S DI, PA enable, LCD reset, a second touch
pad, codec power control), so these demos need changes to run on a v1.2 board.
Espressif's C BSP (`esp-bsp/bsp/esp_vocat`) targets v1.2.

## Requirements

- The Xtensa Rust toolchain installed with [`espup`](https://github.com/esp-rs/espup),
  with `~/export-esp.sh` sourced in the shell.
- [`espflash`](https://github.com/esp-rs/espflash) 4.x.
- A USB-C data cable. The USB-C port is the ESP32-S3 native USB (USB-Serial-JTAG); it is
  used for flashing and for the console. If a battery is installed, press the power
  button so the board is on.

## Running a demo

Every demo is its own binary in `src/bin/`:

```bash
cargo run --release --bin 00_bringup
```

The cargo runner (`.cargo/config.toml`) flashes the board and then opens the serial
monitor. Press `Ctrl+R` to reset the board and `Ctrl+C` to quit. If flashing fails,
hold **BOOT**, press **RST**, then release **BOOT** to enter download mode by hand.

Log verbosity follows `ESP_LOG` (default `info`), for example
`ESP_LOG=debug cargo run --release --bin 00_bringup`.

The console only uses USB-Serial-JTAG. UART0 TX (GPIO43) drives the green LED on this
board, so `esp-println` is built with the `jtag-serial` feature instead of `auto`.

## Project layout

| Path | Contents |
|------|----------|
| `src/board.rs` | Board pin map, I2C device addresses, common `init()`, shared I2C bus, peripheral power switch, green LED |
| `src/bin/NN_*.rs` | The demos |

## Demos

### 00 – Board bring-up

```bash
cargo run --release --bin 00_bringup
```

Checks that the basic hardware is alive:

1. Prints the chip revision, the Wi-Fi station MAC address and memory information from
   eFuse. The flash type tells the board revisions apart: VoCat v1.0 uses an
   ESP32-S3-WROOM-2 with octal flash, v1.2 a WROOM-1 with quad flash.
2. Detects the PSRAM, writes a pattern over all of it, reads it back and reports errors
   and throughput.
3. Adds the PSRAM to the heap and allocates 1 MiB from it.
4. Switches on `VCC_3V3` (GPIO9, active low), which powers the LCD, its touch controller
   and the microSD slot, then pulses the shared LCD/touch reset (GPIO3).
5. Scans the shared I2C bus (SCL GPIO1, SDA GPIO2) and names the devices it knows.
6. Reads the chip ID of every expected device, plus the battery voltage, current,
   charge and temperature from the BQ27220 fuel gauge.
7. Blinks the green LED (GPIO43, active low) once per second and prints the uptime
   every 10 s.

Expected I2C devices:

| Address | Device | ID check |
|---------|--------|----------|
| 0x15 | CST816S touch controller | register 0xA7 = B4 (CST816S) |
| 0x18 | ES8311 speaker codec | registers 0xFD/0xFE = 83 11 |
| 0x40 | ES7210 microphone ADC | registers 0x3D/0x3E = 72 10 |
| 0x55 | BQ27220 fuel gauge | `DEVICE_NUMBER` = 0220 |
| 0x68 | BMI270 IMU | register 0x00 = 24 |

The touch controller goes to sleep a few seconds after reset and then stops answering
on I2C until the screen is touched. The demo releases the reset right before the scan
for that reason.
