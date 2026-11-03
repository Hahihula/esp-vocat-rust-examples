//! Demo 00: board bring-up.
//!
//! Prints chip and memory information from eFuse, tests and maps PSRAM into
//! the heap, powers the LCD rail, scans the shared I2C bus (SCL GPIO1, SDA
//! GPIO2) and reads an ID from every device it expects there.

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

extern crate alloc;

use alloc::vec::Vec;
use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Timer};
use esp_backtrace as _;
use esp_hal::{
    Async,
    efuse::{self, InterfaceMacAddress},
    gpio::{Level, Output, OutputConfig},
    i2c::master::{AcknowledgeCheckFailedReason, Error as I2cError, I2c},
    psram::Psram,
    timer::timg::TimerGroup,
};
use log::{info, warn};
use vocat_rust_examples::board::{self, GreenLed, i2c_addr};

esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    let peripherals = board::init();

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    let mut led = GreenLed::new(peripherals.GPIO43);
    led.set(true);

    info!("=== ESP-VoCat bring-up ===");
    print_chip_info();

    let psram = Psram::new(peripherals.PSRAM, Default::default());
    test_psram(&psram);
    esp_alloc::psram_allocator!(&psram);
    check_heap();

    // The touch controller sits on the LCD flex and is powered by VCC_3V3. It
    // shares its reset line with the LCD (GPIO3) and only answers on I2C for
    // a few seconds after reset, before it goes to sleep.
    let _power = board::peripheral_power_on(peripherals.GPIO9);
    Timer::after(Duration::from_millis(20)).await;
    let mut lcd_rst = Output::new(peripherals.GPIO3, Level::Low, OutputConfig::default());
    Timer::after(Duration::from_millis(10)).await;
    lcd_rst.set_high();
    Timer::after(Duration::from_millis(100)).await;
    info!("VCC_3V3 (LCD, touch, SD) on, LCD/touch reset released");

    match board::i2c(peripherals.I2C0, peripherals.GPIO1, peripherals.GPIO2) {
        Ok(mut i2c) => {
            scan_i2c(&mut i2c).await;
            identify_devices(&mut i2c).await;
        }
        Err(e) => warn!("I2C init failed: {e:?}"),
    }

    info!("Bring-up done. The green LED blinks once per second.");
    let mut seconds = 0u32;
    loop {
        led.toggle();
        Timer::after(Duration::from_millis(500)).await;
        led.toggle();
        Timer::after(Duration::from_millis(500)).await;
        seconds += 1;
        if seconds.is_multiple_of(10) {
            info!("alive, uptime {} s", Instant::now().as_secs());
        }
    }
}

fn print_chip_info() {
    let rev = efuse::chip_revision();
    info!("Chip revision: v{}.{}", rev.major, rev.minor);
    info!(
        "Wi-Fi STA MAC: {}",
        efuse::interface_mac_address(InterfaceMacAddress::Station)
    );

    // The ROM boots octal flash in OPI mode based on this bit. WROOM-2 (VoCat
    // v1.0) has octal flash, WROOM-1 (v1.2) has quad flash.
    let octal_flash = efuse::read_bit(efuse::FLASH_TYPE);
    info!(
        "Flash type (eFuse): {}",
        if octal_flash {
            "octal (WROOM-2, VoCat v1.0?)"
        } else {
            "quad (WROOM-1, VoCat v1.2?)"
        }
    );

    let psram_cap = efuse::read_field_le::<u8>(efuse::PSRAM_CAP)
        | (u8::from(efuse::read_bit(efuse::PSRAM_CAP_3)) << 2);
    let psram_vendor = efuse::read_field_le::<u8>(efuse::PSRAM_VENDOR);
    info!(
        "eFuse: package {}, in-package PSRAM capacity code {psram_cap}, PSRAM vendor {psram_vendor}, \
         flash capacity code {}",
        efuse::read_field_le::<u8>(efuse::PKG_VERSION),
        efuse::read_field_le::<u8>(efuse::FLASH_CAP),
    );
}

/// Writes an address-derived pattern over the whole PSRAM and reads it back.
fn test_psram(psram: &Psram) {
    let (start, size) = psram.raw_parts();
    info!("PSRAM: {} KiB mapped at {:p}", size / 1024, start);
    if size == 0 {
        warn!("PSRAM not detected");
        return;
    }

    let words = size / 4;
    let base = start.cast::<u32>();
    let pattern = |i: usize| (i as u32).wrapping_mul(0x9E37_79B9) ^ 0xA5A5_5A5A;

    let t0 = Instant::now();
    for i in 0..words {
        // SAFETY: `i` stays inside the mapped PSRAM region, which nothing else uses yet.
        unsafe { base.add(i).write_volatile(pattern(i)) };
    }
    let write_ms = t0.elapsed().as_millis();

    let t0 = Instant::now();
    let errors = (0..words)
        // SAFETY: as above.
        .filter(|&i| unsafe { base.add(i).read_volatile() } != pattern(i))
        .count();
    let read_ms = t0.elapsed().as_millis();

    let mib = size as u64 / (1024 * 1024);
    info!(
        "PSRAM test: {errors} bad words, write {write_ms} ms (~{} MiB/s), read {read_ms} ms (~{} MiB/s)",
        mib * 1000 / write_ms.max(1),
        mib * 1000 / read_ms.max(1),
    );
}

fn check_heap() {
    let buf: Vec<u8> = alloc::vec![0x5A; 1024 * 1024];
    let ok = buf.iter().all(|&b| b == 0x5A);
    info!(
        "Heap: 1 MiB allocation at {:p} {}",
        buf.as_ptr(),
        if ok { "OK" } else { "CORRUPT" }
    );
    drop(buf);
    info!("{}", esp_alloc::HEAP.stats());
}

#[allow(
    clippy::large_stack_frames,
    reason = "the async I2C transfer future is ~1 KiB; awaited once from main"
)]
async fn scan_i2c(i2c: &mut I2c<'_, Async>) {
    info!("Scanning I2C bus (SCL=GPIO1, SDA=GPIO2)...");
    let mut found: u128 = 0;
    for addr in 0x08u8..0x78 {
        let mut byte = [0u8; 1];
        match i2c.read_async(addr, &mut byte).await {
            Ok(()) => found |= 1 << addr,
            Err(I2cError::AcknowledgeCheckFailed(AcknowledgeCheckFailedReason::Address)) => {}
            // The address was acknowledged, only the read was refused.
            Err(I2cError::AcknowledgeCheckFailed(_)) => found |= 1 << addr,
            Err(e) => warn!("  0x{addr:02X}: error {e:?}"),
        }
    }

    for addr in (0x08u8..0x78).filter(|&a| found & (1 << a) != 0) {
        info!("  0x{addr:02X}: {}", device_name(addr));
    }
    info!("I2C scan: {} device(s)", found.count_ones());

    for addr in [
        i2c_addr::ES8311,
        i2c_addr::CST816S,
        i2c_addr::ES7210,
        i2c_addr::BQ27220,
        i2c_addr::BMI270,
    ] {
        if found & (1 << addr) == 0 {
            warn!("  expected {} at 0x{addr:02X} did not answer", device_name(addr));
        }
    }
}

fn device_name(addr: u8) -> &'static str {
    match addr {
        i2c_addr::ES8311 => "ES8311 speaker codec",
        i2c_addr::CST816S => "CST816S touch controller",
        i2c_addr::ES7210 => "ES7210 microphone ADC",
        i2c_addr::BQ27220 => "BQ27220 fuel gauge",
        i2c_addr::BMI270 => "BMI270 IMU",
        _ => "unknown",
    }
}

#[allow(
    clippy::large_stack_frames,
    reason = "the async I2C transfer future is ~1 KiB"
)]
async fn read_regs<const N: usize>(
    i2c: &mut I2c<'_, Async>,
    addr: u8,
    reg: u8,
) -> Result<[u8; N], I2cError> {
    let mut buf = [0u8; N];
    i2c.write_read_async(addr, &[reg], &mut buf).await?;
    Ok(buf)
}

#[allow(
    clippy::large_stack_frames,
    reason = "several async I2C transfer futures; awaited once from main"
)]
async fn identify_devices(i2c: &mut I2c<'_, Async>) {
    info!("Reading device IDs...");

    // ES8311: CHIP_ID1/2 at 0xFD/0xFE read 0x83 0x11, version at 0xFF.
    match (
        read_regs::<1>(i2c, i2c_addr::ES8311, 0xFD).await,
        read_regs::<1>(i2c, i2c_addr::ES8311, 0xFE).await,
        read_regs::<1>(i2c, i2c_addr::ES8311, 0xFF).await,
    ) {
        (Ok([id1]), Ok([id2]), Ok([ver])) => info!(
            "  ES8311: chip ID {id1:02X} {id2:02X}, version {ver:02X} {}",
            check(id1 == 0x83 && id2 == 0x11)
        ),
        _ => warn!("  ES8311: no answer"),
    }

    // ES7210: chip ID at 0x3D/0x3E reads 0x72 0x10.
    match (
        read_regs::<1>(i2c, i2c_addr::ES7210, 0x3D).await,
        read_regs::<1>(i2c, i2c_addr::ES7210, 0x3E).await,
    ) {
        (Ok([id1]), Ok([id0])) => info!(
            "  ES7210: chip ID {id1:02X} {id0:02X} {}",
            check(id1 == 0x72 && id0 == 0x10)
        ),
        _ => warn!("  ES7210: no answer"),
    }

    // BMI270: CHIP_ID register 0x00 reads 0x24.
    match read_regs::<1>(i2c, i2c_addr::BMI270, 0x00).await {
        Ok([id]) => info!("  BMI270: chip ID {id:02X} {}", check(id == 0x24)),
        Err(_) => warn!("  BMI270: no answer"),
    }

    // CST816S: chip ID at 0xA7 (B4 = CST816S, B5 = CST816T, B6 = CST816D),
    // firmware version at 0xA9.
    match (
        read_regs::<1>(i2c, i2c_addr::CST816S, 0xA7).await,
        read_regs::<1>(i2c, i2c_addr::CST816S, 0xA9).await,
    ) {
        (Ok([id]), Ok([fw])) => {
            let name = match id {
                0xB4 => "CST816S",
                0xB5 => "CST816T",
                0xB6 => "CST816D",
                _ => "unknown model",
            };
            info!("  Touch: chip ID {id:02X} ({name}), firmware {fw:02X}");
        }
        _ => warn!("  Touch: no answer (it sleeps a few seconds after reset; touch the screen and reset)"),
    }

    identify_fuel_gauge(i2c).await;
}

#[allow(
    clippy::large_stack_frames,
    reason = "several async I2C transfer futures; awaited once from main"
)]
async fn identify_fuel_gauge(i2c: &mut I2c<'_, Async>) {
    let addr = i2c_addr::BQ27220;

    // Control() subcommand DEVICE_NUMBER (0x0001); the answer appears in
    // MACData (0x40) and reads 0x0220.
    let device = async {
        i2c.write_async(addr, &[0x00, 0x01, 0x00]).await?;
        Timer::after(Duration::from_millis(5)).await;
        read_regs::<2>(i2c, addr, 0x40).await
    }
    .await;
    match device {
        Ok(id) => {
            let id = u16::from_le_bytes(id);
            info!("  BQ27220: device number {id:04X} {}", check(id == 0x0220));
        }
        Err(_) => {
            warn!("  BQ27220: no answer");
            return;
        }
    }

    // Standard commands, all 16-bit little endian.
    let word = |b: [u8; 2]| u16::from_le_bytes(b);
    let (Ok(temp), Ok(volt), Ok(curr), Ok(soc), Ok(rem), Ok(full)) = (
        read_regs::<2>(i2c, addr, 0x06).await.map(word),
        read_regs::<2>(i2c, addr, 0x08).await.map(word),
        read_regs::<2>(i2c, addr, 0x0C).await.map(word),
        read_regs::<2>(i2c, addr, 0x2C).await.map(word),
        read_regs::<2>(i2c, addr, 0x10).await.map(word),
        read_regs::<2>(i2c, addr, 0x12).await.map(word),
    ) else {
        warn!("  BQ27220: reading battery data failed");
        return;
    };
    // Temperature is in 0.1 K.
    let deci_c = i32::from(temp) - 2731;
    info!(
        "  Battery: {volt} mV, {} mA, {soc} %, {rem}/{full} mAh, {}.{} °C",
        curr as i16,
        deci_c / 10,
        (deci_c % 10).abs()
    );
}

fn check(ok: bool) -> &'static str {
    if ok { "(OK)" } else { "(unexpected)" }
}
