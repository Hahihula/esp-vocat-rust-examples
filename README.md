# ESP-VoCat Rust examples

Bare-metal (`no_std`) Rust demos for the
[ESP-VoCat v1.2](https://docs.espressif.com/projects/esp-dev-kits/en/latest/esp32s3/esp-vocat/user_guide_v1.2.html)
AI development kit (ESP32-S3, 1.85" round QSPI touch LCD, dual microphones, speaker,
IMU, fuel gauge, microSD), built on [esp-hal](https://github.com/esp-rs/esp-hal) and
Embassy.

The pin map in `src/board.rs` follows the
[v1.2 schematic](https://dl.espressif.com/AE/esp-dev-kits/ESP-VoCat_SCH_V1_2.pdf), which
matches Espressif's C BSP (`esp-bsp/bsp/esp_vocat`). ESP-VoCat v1.0 uses other pins for
UART1, I2S DI, the PA enable and the LCD reset, has one touch pad instead of two and no
codec power switch, so these demos need changes to run on a v1.0 board. Demo 00 tells the
revisions apart: v1.2 has an ESP32-S3-WROOM-1 with quad flash, v1.0 a WROOM-2 with octal
flash.

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

Every demo waits 2 s after boot (`board::wait_for_console()`) before printing. The
USB-Serial-JTAG console re-enumerates on every reset, and without the wait the first
lines are lost when the monitor reconnects.

PSRAM runs at 80 MHz (`board::psram()`), twice the esp-hal default. Demo 00 measures
about 33 MiB/s for both reads and writes, with no errors.

The console only uses USB-Serial-JTAG. UART0 TX (GPIO43) drives the green LED on this
board, so `esp-println` is built with the `jtag-serial` feature instead of `auto`.

## Project layout

| Path | Contents |
|------|----------|
| `src/board.rs` | Board pin map, I2C device addresses, common `init()`, shared I2C bus, LCD/SD and codec power switches, LCD reset, green LED |
| `src/audio.rs` | I2S output stream to the ES8311 and input stream from the ES7210 (DMA ring buffers; output restarts itself after running dry), and a small synthesiser |
| `src/battery.rs` | BQ27220 fuel gauge driver (generic over `embedded-hal-async` I2C) |
| `src/motion.rs` | Software tilt, moving/still, shake and knock detection from IMU samples |
| `src/es7210.rs` | ES7210 microphone ADC driver, ported from Espressif's `esp_codec_dev` |
| `src/display.rs` | ST77916 QSPI LCD driver, PSRAM frame buffer (`embedded-graphics` `DrawTarget`) and PWM backlight |
| `src/st77916_init.rs` | ST77916 vendor init sequence from Espressif's BSP |
| `src/touchscreen.rs` | CST816T touch controller driver (position, controller gestures) |
| `src/touch.rs` | Register-level driver for the two touch pads, baseline tracking and tap/long/swipe decoding |
| `src/net.rs` | Wi-Fi station with reconnect, embassy-net stack with DHCP, SNTP client |
| `src/clock.rs` | Local time from Unix time: fixed UTC offset plus EU summer time, weekday |
| `src/radio.rs` | Internet radio: HTTP stream download, MP3 decoding (`nanomp3-core`), resampling and volume ramp |
| `src/bin/NN_*.rs` | The demos |

## Demos

### 00 – Board bring-up

```bash
cargo run --release --bin 00_bringup
```

Checks that the basic hardware is alive:

1. Prints the chip revision, the Wi-Fi station MAC address and memory information from
   eFuse. The flash type tells the board revisions apart (see above).
2. Detects the PSRAM, writes a pattern over all of it, reads it back and reports errors
   and throughput.
3. Adds the PSRAM to the heap and allocates 1 MiB from it.
4. Switches on `VCC_3V3` (GPIO9, active low), which powers the LCD, its touch controller
   and the microSD slot, and `CODEC_3V3` (GPIO48, active high), which powers the ES8311
   and ES7210. Then pulses the shared LCD/touch reset (GPIO47, which drives a transistor,
   so high means reset).
5. Scans the shared I2C bus (SCL GPIO1, SDA GPIO2) and names the devices it knows.
6. Reads the chip ID of every expected device, plus the battery voltage, current,
   charge and temperature from the BQ27220 fuel gauge.
7. Blinks the green LED (GPIO43, active low) once per second and prints the uptime
   every 10 s.

Expected I2C devices:

| Address | Device | ID check |
|---------|--------|----------|
| 0x15 | CST816T touch controller | register 0xA7 = B5 (CST816T) |
| 0x18 | ES8311 speaker codec | registers 0xFD/0xFE = 83 11 |
| 0x40 | ES7210 microphone ADC | registers 0x3D/0x3E = 72 10 |
| 0x55 | BQ27220 fuel gauge | `DEVICE_NUMBER` = 0220 |
| 0x68 | BMI270 IMU | register 0x00 = 24 |

The touch controller goes to sleep a few seconds after reset and then stops answering
on I2C until the screen is touched. The demo releases the reset right before the scan
for that reason.

Expected output (verified on a v1.2 board with a battery; the esp-hal PSRAM driver also
prints its own detection lines, left out here):

```
INFO - === ESP-VoCat bring-up ===
INFO - Chip revision: v0.2
INFO - Wi-Fi STA MAC: 90:e5:b1:..:..:..
INFO - Flash type (eFuse): quad (WROOM-1, VoCat v1.2)
INFO - eFuse: package 0, in-package PSRAM capacity code 3, PSRAM vendor 2, flash capacity code 0
INFO - PSRAM initialized successfully in Octal SPI mode
INFO - PSRAM size: 16 MB
INFO - PSRAM: 16384 KiB mapped at 0x3c020000
INFO - PSRAM test: 0 bad words, write 478 ms (~33 MiB/s), read 471 ms (~33 MiB/s)
INFO - Heap: 1 MiB allocation at 0x3c020000 OK
INFO - VCC_3V3 (LCD, touch, SD) and CODEC_3V3 on, LCD/touch reset released
INFO - Scanning I2C bus (SCL=GPIO1, SDA=GPIO2)...
INFO -   0x15: CST816 touch controller
INFO -   0x18: ES8311 speaker codec
INFO -   0x40: ES7210 microphone ADC
INFO -   0x55: BQ27220 fuel gauge
INFO -   0x68: BMI270 IMU
INFO - I2C scan: 5 device(s)
INFO - Reading device IDs...
INFO -   ES8311: chip ID 83 11, version 01 (OK)
INFO -   ES7210: chip ID 72 10 (OK)
INFO -   BMI270: chip ID 24 (OK)
INFO -   Touch: chip ID B5 (CST816T), firmware 02
INFO -   BQ27220: device number 0220 (OK)
INFO -   Battery: 3859 mV, 380 mA, 100 %, 650/650 mAh, 40.5 °C
INFO - Bring-up done. The green LED blinks once per second.
INFO - alive, uptime 11 s
```

Without `CODEC_3V3` (GPIO48) the ES8311 and ES7210 do not answer at all. At the esp-hal
default PSRAM clock of 40 MHz the same test gave only 17 MiB/s writes and 26 MiB/s reads.

### 01 – Touch pads

```bash
cargo run --release --bin 01_touch_pads
```

The BOOT and RST buttons sit inside the housing, and the only reachable button is the
power button, so the demos use the two capacitive touch pads as input: TOUCH_PAD1
(GPIO6) at the back of the cat's head and TOUCH_PAD2 (GPIO7) at the front. A swipe
goes from back to front or from front to back.

esp-hal has no touch sensor driver for the ESP32-S3. `src/touch.rs` programs the touch
FSM directly, following ESP-IDF's `touch_ll`: timer mode, 500 measurement cycles per
channel, charge between 0.5 V and 2.7 V, both channels scanned continuously. The CPU
only reads the raw counts.

On top of the raw counts, `TouchInput`:

1. Measures a baseline per pad at start-up. Don't touch the pads during the first
   ~200 ms.
2. Polls every 20 ms, smooths the readings and follows slow baseline drift while a pad
   is untouched. Rising readings are only followed within 5 ‰ of the baseline, so a
   hovering finger doesn't drag the baseline up.
3. Reports a touch when the reading rises 40 ‰ above the baseline and a release when it
   falls below 20 ‰.
4. Decodes gestures:

| Event | Meaning |
|-------|---------|
| `Tap(pad)` | Short touch, reported 400 ms after it started if no swipe followed |
| `Long(pad)` | Held for 0.8 s, reported while still held |
| `Swipe { from, to }` | The other pad touched within 400 ms of the first |
| `Down(pad)` / `Up(pad, held)` | Raw touch start and end |

The demo prints the raw count and baseline of both pads once per second, logs every
event with counters and turns the green LED on while a pad is touched.

Measured on the board: the idle counts are about 26500 (back) and 28500 (front). A finger
on a pad raises its count by 50–85 ‰. The neighbouring pad also rises by 10–30 ‰
through the housing, which is why the touch threshold is 40 ‰ and not lower.

Example output:

```
INFO - === Demo 01: touch pads (TOUCH6/GPIO6, TOUCH7/GPIO7) ===
INFO - Calibrating, don't touch the pads...
INFO - back baseline 26593
INFO - front baseline 28572
INFO - Tap, hold or swipe from one pad to the other.
INFO - raw/baseline: back 26566/26564 (+0 ‰) | front 28552/28553 (+0 ‰)
INFO - front down
INFO - front up after 220 ms
INFO - TAP   front #1
INFO - front down
INFO - back down
INFO - SWIPE front -> back #1
INFO - front up after 419 ms
INFO - back up after 379 ms
INFO - back down
INFO - raw/baseline: back 29062/26690 (+89 ‰) TOUCHED | front 28973/28759 (+7 ‰)
INFO - LONG  back #1
INFO - back up after 839 ms
```

### 02 – Battery and fuel gauge

```bash
cargo run --release --bin 02_battery
```

Reads the BQ27220 fuel gauge (I2C 0x55) every 5 s: voltage, current, average current
and power, charge, health, remaining and full capacity, temperature, time to empty or
full, cycle count and status flags. The TP4057 charger's status pins only drive the
charge LED, so the gauge current is the only way to tell the charging state:

| State | Current | Green LED |
|-------|---------|-----------|
| charging | above +15 mA | blinks at 1 Hz |
| idle (full, or noise) | within ±15 mA | steady on |
| discharging (on battery) | below −15 mA | short flash every 3 s |

Touch pads:

- **front tap:** full gauge dump
- **back tap:** history of the last 32 state changes

The console is USB, so it disappears while the cable is unplugged. To see what happened
on battery, unplug USB (the cat keeps running if it is powered on), wait, and plug it back
in. When charging starts again the demo prints how long it ran on battery, the capacity
used, the charge change and the lowest voltage, followed by the history. The monitor
needs to be restarted after replugging (`espflash monitor`).

Expected output (on USB, battery charging):

```
INFO - === Demo 02: battery (BQ27220 fuel gauge) ===
INFO - front tap = full dump, back tap = state history
INFO - LED: blinking = charging, steady = idle/full, short flash every 3 s = on battery
INFO - BQ27220 found, firmware 2002
INFO - 4031 mV  +379 mA (avg +379)  100 %  650/650 mAh  44.9 °C, full in 0:00 h
INFO - state: start -> charging
INFO - 4032 mV  +379 mA (avg +379)  100 %  650/650 mAh  44.9 °C, full in 0:00 h
```

Observations on the test board:

- **Charge level:** the gauge reports 100 % while the cell still charges at a constant
  ~380 mA at only 4.03 V. A Li-ion cell isn't full at that point, so the charge
  percentage looks uncalibrated. Voltage and current are measured directly and are
  reliable. Reconfiguring the gauge needs unsealed data-memory writes and is out of
  scope here.
- **Temperature:** the BQ27220 has no thermistor on this board, so it reports its own
  chip temperature. Inside the closed housing next to the charger, that reached about
  45 °C while charging.
- **Charge inhibit:** at that temperature the flags (0x4368) include `charge-inhibit`, most
  likely the gauge's default 45 °C charge limit. The flag is information only; the TP4057
  doesn't read it.
- **Current and power disagree:** the gauge reports +379 mA at 4.04 V but an average power
  of only 153 mW, which matches about 38 mA. The current also stays at exactly 379 mA with
  no noise. One of the two registers is probably mis-scaled by the gauge's default
  calibration; which one is still open.

Full dump (front tap):

```
INFO - --- fuel gauge ---
INFO - voltage          4043 mV
INFO - current          +379 mA (average +379 mA)
INFO - average power    +153 mW
INFO - state of charge  100 %
INFO - state of health  100 %
INFO - capacity         650 mAh remaining, 650 mAh full, 650 mAh design
INFO - temperature      45.3 °C
INFO - time to empty    -
INFO - time to full     0:00 h
INFO - cycle count      0
INFO - flags 4368:     present full charge-inhibit
```

### 03 – IMU

```bash
cargo run --release --bin 03_imu
```

The BMI270 (I2C 0x68) produces no data until the host uploads Bosch's 8 KB feature-engine
configuration. The demo uses the async BMI270 driver from
[`oxstack-drivers`](https://crates.io/crates/oxstack-drivers), which bundles that file
(BSD-3-Clause, © Bosch Sensortec) and streams it in 256-byte chunks. That takes about
210 ms at 400 kHz. The driver only polls; the INT1 line (GPIO21) is not used yet.

The demo runs the sensor at ±4 g, ±1000 °/s and 200 Hz, reads it every 5 ms and feeds the
samples into `MotionDetector` (`src/motion.rs`):

| Event | Rule |
|-------|------|
| `TILTED` / `UPRIGHT` | Angle between gravity and the upright reference above 35° / below 20°, only from samples with \|a\| within 0.15 g of 1 g |
| `UPSIDE DOWN` | Tilt above 140° |
| `MOVING` / `STILL` | Rotation above 25 °/s / below 8 °/s for 1 s |
| `SHAKE` | Three peaks of \|a\| above 1.8 g within 0.8 s, with \|a\| falling below 1.3 g between them |
| `KNOCK` | Acceleration change of more than 0.35 g between two samples, with less than 60 °/s of rotation |

The position at start-up is the upright reference, so stand the cat upright and keep it
still for the first second. Upright, the sensor reads about [0, 0, −1] g, so the cat's
"up" is the sensor's −Z axis.

The green LED is on while the cat is moving. Touch pads:

- **front tap:** take the current position as upright
- **back tap:** detailed reading with the IMU die temperature

A status line is printed once per second. Its "peak jerk" is the largest change of
acceleration between two samples since the previous line, which helps tune the knock
threshold. Measured on the test board: knocks on the housing 0.4–1.3 g, handling 0.1–0.3 g,
shaking by hand 2–4.5 g with jerks up to 5.7 g.

All events were checked on the test board, including upside down and shaking.

Example output (tilting, then knocking on the still cat):

```
INFO - === Demo 03: BMI270 IMU (I2C 0x68) ===
INFO - front tap = set upright, back tap = detailed reading
INFO - BMI270 ready after 210 ms (config upload), ±4 g, ±1000 °/s, 200 Hz
INFO - upright reference: [-0.01 +0.02 -1.00] g (sensor axes)
INFO - acc [-0.01 +0.02 -1.00] g |a| 1.00  gyro [  -0.4   +0.2   -0.2] °/s  tilt   0.0°  peak jerk 0.01 g
INFO - MOVING
INFO - TILTED 40°
INFO - acc [-0.74 -0.38 -0.24] g |a| 0.86  gyro [  +7.0 +225.9 +201.2] °/s  tilt  73.7°  peak jerk 0.26 g
INFO - UPRIGHT
INFO - KNOCK (1.30 g)
INFO - acc [-0.03 +0.01 -0.99] g |a| 0.99  gyro [  -0.4   +0.2   -0.0] °/s  tilt   0.8°  peak jerk 1.30 g
INFO - STILL
```

Turning it over and shaking it:

```
INFO - TILTED 36°
INFO - acc [+0.33 -0.82 +0.26] g |a| 0.92  gyro [+116.9 +167.6  -10.6] °/s  tilt 106.4°  peak jerk 0.07 g
INFO - UPSIDE DOWN
INFO - TILTED 118°
INFO - SHAKE
INFO - SHAKE
INFO - UPRIGHT
```

### 04 – Round LCD

```bash
cargo run --release --bin 04_display
```

The 1.85" 360×360 round panel has an ST77916 controller on QSPI (CLK GPIO18, D0–D3
GPIO46/13/11/12, CS GPIO14, DC GPIO45, reset GPIO47, backlight GPIO44). No Rust crate
supports it, so `src/display.rs` implements the protocol of Espressif's
`esp_lcd_st77916` driver. While CS is low:

1. A 32-bit header goes out on one data line with DC low: `02 00 cmd 00` for a command,
   `32 00 2C 00` (RAMWR) for pixels.
2. DC goes high, then either the command parameters follow on one line, or the pixels
   (RGB565, high byte first) on all four lines.

The init sequence is MADCTL, COLMOD (16 bit), Espressif's vendor table from the BSP, INVON
and DISPON. CS is a plain GPIO so it can stay low across the transfers of a frame.

Pixel data goes through `SpiDma` (SPI2, DMA channel 0). Each chunk of at most 32 KB is
first copied by the CPU into a buffer in internal RAM and sent from there. DMA directly
out of the PSRAM frame buffer showed stale data: rows redrawn every frame (the eyes in
demo 06) came out with the picture an earlier program had left in PSRAM. esp-hal writes
the cache back before such a transfer, yet the stale rows still appeared. The CPU copy
reads through the cache, so it always sends the current pixels.

Drawing uses [embedded-graphics](https://crates.io/crates/embedded-graphics) on a
`FrameBuffer`, a full 253 KiB RGB565 frame in PSRAM. It tracks which rows changed, and
`Lcd::flush` sends only that band of rows, which is contiguous in memory. The backlight
is LEDC PWM at 20 kHz (`Backlight::set`, `Backlight::fade`).

Screens (front tap = next screen, back tap = brightness 100/50/20/5 %):

1. **Test pattern:** colour bars, a grey ramp, a circle along the round edge, a centre dot
   and text.
2. **Solid colours:** red, green, blue, white, black, one per second. Use it to check the
   colour order and inversion.
3. **Bouncing ball:** reflects off the round edge, and only the changed rows are sent.
4. **Benchmark:** a scrolling gradient, every frame redrawn and sent in full.

Measured at a 40 MHz SPI clock: a full frame takes 18 ms to send (about 13 MiB/s), and
the benchmark runs at 36 fps, with 7 ms of drawing and 18 ms of sending per frame. Before
the bounce buffer it was 14 ms and 42 fps; the CPU copy out of PSRAM costs about 4 ms
per full frame. The bus itself allows 20 MiB/s. At 80 MHz a frame took only 8 ms and the benchmark reached 59 fps, but the picture was
unrecognisable. The LCD pins are routed through the GPIO matrix rather than IO_MUX,
which doesn't keep up. The clock source is 80 MHz with integer dividers, so 40 MHz is the
next usable setting.

Transfers are safe to cancel. A screen is a future that is dropped on a tap, often in the
middle of a frame. Without the guard that raises CS on drop, CS stayed low and the panel
read the next commands as pixel data, so the screen froze. Now an aborted frame just
ends early.

Expected output:

```
INFO - === Demo 04: round LCD 360x360 (ST77916, QSPI) ===
INFO - front tap = next screen, back tap = brightness
INFO - PSRAM heap: 16384 KiB (80 MHz)
INFO - LCD initialised, SPI clock 40 MHz
INFO - full frame (253 KiB) sent in 18 ms (~13 MiB/s)
INFO - screen: TestPattern
```

On the benchmark screen:

```
INFO -   benchmark: 36 fps, per frame 7 ms drawing + 18 ms sending
```

### 05 – Touchscreen paint

```bash
cargo run --release --bin 05_paint
```

Finger painting on the round LCD. The CST816T touch controller (I2C 0x15, INT GPIO10)
shares its reset line with the LCD, so `Lcd::new` resets both. Right after that,
`Cst816::init` disables the controller's auto-sleep (register 0xFE); otherwise it stops
answering on I2C a few seconds after reset. It also enables INT pulses on touch and on
movement (0xFA = 0x60) and double-click detection (0xEC).

The demo waits for a falling edge on INT and polls every 15 ms while a finger is down.
Each report gives one point. Consecutive points are joined with a line of the brush
width and a round cap, so fast strokes stay continuous. Only the changed rows are sent
to the LCD.

- **Colour:** touch one of the seven dots along the bottom edge (a touch that starts on
  the palette doesn't paint)
- **front pad tap:** clear the canvas
- **back pad tap:** next brush size (4, 8, 14, 24 px), previewed at the top

The touch coordinates match the display orientation without any mapping (0..359,
origin top left). The controller also reports its own gestures (click, swipes, long press),
which the demo logs.

Example output:

```
INFO - === Demo 05: touchscreen paint (CST816T) ===
INFO - paint with a finger, touch a colour dot to pick it
INFO - front pad tap = clear, back pad tap = brush size
INFO - touch controller: chip ID B5, firmware 02
INFO - down at ( 89, 292)
INFO - colour 0
INFO - down at (112, 231)
INFO - gesture SwipeUp at (125, 195)
INFO - up   at (142, 162), 8 points in 168 ms
INFO - down at ( 75, 108)
INFO - gesture SwipeRight at (118, 106)
INFO - up   at (248,  93), 8 points in 168 ms
```

### 06 – Googly cat eyes

```bash
cargo run --release --bin 06_cat_eyes
```

Two cat eyes on the round LCD whose pupils are moved by the accelerometer, like googly
eyes. Each frame (every 20 ms) the demo reads the BMI270 and takes "down" in screen
coordinates from it. The accelerometer measures the reaction to gravity plus the opposite
of any motion, so the pupils both fall towards the ground and swing when the cat is moved.
Each pupil is a damped point mass pulled by that vector and bounces off the edge of the
eye.

Upright, the sensor reads [0, 0, −1] g, so screen-down is the sensor's +Z and
screen-right its +X (`SCREEN_X`, `SCREEN_Y` in the demo).

| Input | Reaction |
|-------|----------|
| tilt or move the cat | pupils fall towards the lower side and swing |
| touch an eye on the screen | that eye blinks |
| touch elsewhere on the screen | both eyes blink |
| shake (`MotionDetector`) | dizzy, spinning pupils for 2 s |
| back pad tap or hold (petting) | happy `^ ^` eyes for 1.5 s |

The eyes also blink on their own every 2–6 s; an upper and a lower lid close towards the
middle. The face (nose, mouth, whiskers) is drawn once, and each frame redraws only the
eye area.

It runs at the full 50 fps (one frame per 20 ms), including the copy of every frame
through the internal bounce buffer. Example output (upright, then shaken):

```
INFO - === Demo 06: googly cat eyes (BMI270 + LCD) ===
INFO - tilt or move the cat; touch an eye to blink; shake = dizzy; back pad tap = happy
INFO - 50 fps, down on screen (+0.01, +1.00) g
INFO - 50 fps, down on screen (+0.56, -0.12) g
INFO - shake: dizzy
INFO - 50 fps, down on screen (+0.17, +0.85) g
```

This is the first demo with two devices on the I2C bus at the same time: the BMI270 and
the CST816T share it through `embassy-embedded-hal`'s `I2cDevice` over an async
`Mutex`.

### 07 – Speaker

```bash
cargo run --release --bin 07_speaker
```

Plays generated sound through I2S, the ES8311 codec (I2C 0x18) and the NS4150B 3 W
amplifier. The start-up order is:

1. Codec power on (`CODEC_3V3`, GPIO48).
2. I2S0 as master on DMA channel 1: 44.1 kHz, 16-bit stereo, Philips format. MCLK on
   GPIO42, BCLK GPIO40, WS GPIO39, DOUT GPIO41.
3. ES8311 init, using the driver from `oxstack-drivers`. It runs the codec as I2S slave
   with its clock derived from BCLK, so the codec itself needs no MCLK; MCLK is still
   generated for the ES7210 on the same bus. Then DAC volume, playback mode and unmute.
4. Amplifier enable (GPIO15) last, so the codec's power-up doesn't pop.

`AudioOut` streams through an 8 KB DMA ring buffer (about 46 ms). An audio task keeps it
full from `Synth`, a one-voice synthesiser with a sine table, triangle and square waves,
an attack/release envelope, note sequences and exponential sweeps. The main task sends
it commands over a channel. The ring must never run dry: the first version topped it up
by only one descriptor per wake-up, and a short delay (the codec's I2C init) let the DMA
catch up and stop with a descriptor error. `AudioOut::fill` now refills the whole ring
each time, and the ring starts filled with silence.

Touch pads:

- **front tap:** next item: C major scale, "Ode to Joy" (triangle wave), a 100 Hz → 8 kHz
  sweep over 4 s, and A4 (440 Hz) as sine, triangle and square
- **front long touch:** stop
- **back tap:** next codec volume step (−30, −20, −12, −6, 0 dB, starting at −12 dB),
  confirmed with a chime

Expected output:

```
INFO - === Demo 07: speaker (I2S, ES8311, NS4150B) ===
INFO - front tap = next item, front long touch = stop, back tap = volume
INFO - I2S running: 44100 Hz, 16-bit stereo
INFO - ES8311 in playback mode, volume -12 dB
```

### 08 – Microphones

```bash
cargo run --release --bin 08_microphones
```

Records both microphones through the ES7210 (I2C 0x40) and shows them on the LCD. The
ES7210 runs as I2S slave: the ESP32-S3 provides MCLK (256 × fs = 4.096 MHz), BCLK and WS,
and receives 16 kHz, 16-bit stereo on DIN (GPIO3). Left is MIC1, right is MIC2.
`src/es7210.rs` is a port of the init sequence of Espressif's `esp_codec_dev` driver.
Only SDOUT1 of the ES7210 is wired, so in plain I2S mode only MIC1 and MIC2 are
available. MIC3, the speaker loopback for echo cancellation, would need TDM mode.

A task reads the stream and publishes RMS and peak level per channel every 50 ms (800
samples, after a slow DC-removing high-pass). The first 300 samples of each block go to
a shared buffer for the oscilloscope. The main task draws:

- **Oscilloscope:** both channels (MIC1 yellow, MIC2 cyan), about 19 ms wide
- **Level meters:** −60…0 dBFS RMS per microphone, with a 1.5 s red peak-hold marker
- **Gain:** the current microphone gain

It also logs both levels twice a second. A peak above −6 dBFS after at least 300 ms
below −30 dBFS logs `CLAP` and flashes the green LED.

Touch pads:

- **back tap:** next microphone gain (0, 12, 24, 30, 37.5 dB; starts at 30 dB)
- **front tap:** freeze or unfreeze the oscilloscope

Measured in a quiet room, the noise floor follows the gain: about −88 dBFS at 0 dB,
−77 dBFS at 12 dB, −60 dBFS at 30 dB and −53 dBFS at 37.5 dB. A clap at 30 dB reaches
0 dBFS. A sound from one side can show up to 20 dB more on the nearer microphone.

Two pitfalls of the receive stream, both fixed:

- The ring starts filling as soon as `AudioIn::new` returns. If the reading task is
  started later (here it first waited for the ES7210 init), the ring overflows and the
  DMA stops with a descriptor error. The reader is spawned right after the stream is
  created.
- A DMA descriptor doesn't always end on a frame boundary. Consuming only whole 4-byte
  frames left the last bytes of such a descriptor unconsumed forever, so the ring stopped
  draining. `AudioIn::read` consumes everything and carries a split frame over to the
  next piece.

Example output:

```
INFO - === Demo 08: microphones (ES7210, 16 kHz) ===
INFO - back tap = mic gain, front tap = freeze scope; clap to flash the LED
INFO - ES7210 chip ID 72 10
INFO - ES7210 running, gain 30 dB
INFO - MIC1  -62.3 dBFS (peak  -52.1)  MIC2  -62.2 dBFS (peak  -51.6)
INFO - CLAP (0.0 dBFS)
INFO - MIC1  -28.5 dBFS (peak  -12.6)  MIC2  -29.6 dBFS (peak  -17.4)
INFO - gain 0 dB
INFO - MIC1  -88.0 dBFS (peak  -78.3)  MIC2  -88.2 dBFS (peak  -80.8)
```

### 09 – Internet radio alarm clock

```bash
cp .cargo/radio.toml.example .cargo/radio.toml   # then fill in your Wi-Fi
cargo run --release --bin 09_radio_alarm
```

A bedside alarm clock. It gets the time over Wi-Fi and wakes you with an internet radio
stream that starts at −40 dB and rises to full volume over a minute. No SD card is
needed.

**Configuration.** Cargo reads `.cargo/radio.toml` (gitignored) at build time; rebuild
after changing it.

| Variable | Default | Meaning |
|----------|---------|---------|
| `WIFI_SSID`, `WIFI_PASSWORD` | – | 2.4 GHz network, WPA2-Personal, or open with an empty password |
| `RADIO_URL` | `http://ice1.somafm.com/groovesalad-128-mp3` | MP3 stream over plain HTTP (HTTPS is not supported) |
| `TZ_OFFSET_MIN` | `60` | Local standard time offset from UTC in minutes |
| `TZ_EU_DST` | `1` | EU summer time (last Sunday of March to last Sunday of October) |

**How it works.**

- **Time:** `esp-radio` Wi-Fi with automatic reconnect, `embassy-net` with DHCP, and an
  SNTP query to `pool.ntp.org`. The clock runs on the system timer from there and is
  resynced every 6 hours.
- **Download:** `radio::stream_task` runs only while the radio is on. It sends an
  HTTP/1.0 GET (so the body is never chunked) without `Icy-MetaData`, follows up to three
  redirects, and writes the body into a 32 KiB pipe (2 s at 128 kbit/s). It reconnects
  after 3 s if the stream fails.
- **Decoding:** `radio::Player` runs inside the audio task's refill callback. Playback
  starts once 16 KiB is buffered. It decodes one MP3 frame at a time with
  [`nanomp3-core`](https://crates.io/crates/nanomp3-core) (no allocation, about 22 KiB of
  state), mixes stereo to mono and converts other sample rates to 44.1 kHz by linear
  interpolation. The gain follows the ramp linearly in dB. If the buffer runs dry, it
  goes quiet until it has buffered again.
- **Fallback:** while ringing, if the stream produces no audio for 15 s, a square-wave
  beeper is mixed in and rises from 20 % to full over 30 s. It goes off again when the
  stream recovers. The alarm gives up after 30 minutes.

**Screen** (touchscreen):

- **Clock:** 7-segment HH:MM with a blinking colon, a ring of 60 dots for the seconds,
  weekday and date, alarm state and volume. Touch it to open the editor.
- **Editor:** +/− for hours and minutes (hold to repeat), ON/OFF, OK. It closes itself
  after 30 s without a touch.
- **Ringing:** orange "WAKE UP"; touch to stop.
- The backlight dims to 15 % between 22:00 and 07:00, and a touch brings it back to full
  for 20 s.

**Touch pads:**

| | Normal | Ringing |
|---|---|---|
| back tap | alarm on/off | stop |
| front tap | radio on/off (3 s ramp) | snooze 5 minutes |
| front long touch | alarm at the next minute (for testing) | – |
| back long touch | next codec volume step (−30 … 0 dB, starts at −12 dB) | – |

The alarm time is kept in RAM only. After a reset it is 07:00 and off.

**Measured** with the 128 kbit/s stream: the decoder uses 14–16 % of the CPU, about 383
frames per 10 s, with the 32 KiB buffer full and no underruns, including after switching the
radio off and on repeatedly.

Three pitfalls:

- **Wi-Fi start-up starves the audio task.** Starting Wi-Fi keeps the CPU busy for
  longer than the 46 ms audio ring lasts, and the DMA stops with a descriptor error.
  At first that silenced the output for good: the player never ran, and the download
  filled up and stalled. `AudioOut::fill` now restarts the DMA stream after the error.
  The log shows `audio ran dry (DescriptorError), restarted` twice during start-up, and
  never after.
- **Compiler bug at `opt-level = "s"`.** The Xtensa LLVM backend fails on the MP3
  decoder with `Cannot select: i32 = Constant<4608>`, whether it outputs `i16` or `f32`.
  A per-package `opt-level` doesn't help, because fat LTO generates code for everything at
  the binary's level. The release profile therefore uses `opt-level = 2` for all demos.
- **Shared I2C.** The ES8311 and the CST816T are on one bus, shared through
  `embassy-embedded-hal`'s `I2cDevice`. The LCD reset also resets the touch controller,
  so the LCD is set up before the touch controller.

Example output:

```
INFO - Wi-Fi connected to 'Xiaomi_0B86_24' (channel 11, RSSI -25 dBm)
INFO - DHCP: address 192.168.31.131/24
INFO - NTP: Wed 2026-11-04 11:56:07 (UTC+1:00)
INFO - Radio stream: http://ice1.somafm.com/groovesalad-128-mp3
INFO - alarm off
INFO - test: alarm 11:58
INFO - ALARM Wed 2026-11-04 11:58:00
INFO - radio: connecting to ice1.somafm.com (173.239.76.148:80)
INFO - radio: Content-Type: audio/mpeg
INFO - radio: icy-br: 128
INFO - radio: icy-genre: Ambient Chill
INFO - radio: icy-name: Groove Salad: a nicely chilled plate of ambient beats and grooves. [SomaFM]
INFO - radio: buffered, playing
INFO - radio: MPEG layer 3, 128 kbit/s, 44100 Hz, Stereo
INFO - Wed 2026-11-04 11:58:10 | 147 kbit/s, buffer 32 KiB, 341 frames, decode 14% CPU, gain -34.0 dB, underruns 0
INFO - Wed 2026-11-04 11:58:20 | 127 kbit/s, buffer 32 KiB, 383 frames, decode 16% CPU, gain -27.4 dB, underruns 0
INFO - alarm snoozed for 5 minutes
INFO - radio: disconnected
```
