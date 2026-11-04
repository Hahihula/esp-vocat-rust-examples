//! Wi-Fi station plus embassy-net stack, shared by the network demos.
//!
//! Credentials are read at build time from the `WIFI_SSID` and `WIFI_PASSWORD`
//! environment variables (`.cargo/radio.toml`, see `.cargo/radio.toml.example`).
//! An empty or missing password selects an open network.
//!
//! Ported from the ESP32-S31-Korvo-1 examples; [`DateTime`] and the SNTP
//! client are shared with [`crate::clock`].

use embassy_executor::Spawner;
use embassy_net::{
    Config as NetConfig, Runner, Stack, StackResources,
    dns::DnsQueryType,
    udp::{PacketMetadata, UdpSocket},
};
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::peripherals::WIFI;
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config as WifiConfig, ControllerConfig, Interface, WifiController,
    WifiError, sta::StationConfig,
};
use log::{info, warn};
use static_cell::StaticCell;

/// Socket slots: DHCP, DNS, plus a few for the application.
const SOCKETS: usize = 6;
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// A connect attempt normally finishes within a few seconds; give up after this.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Build-time Wi-Fi credentials, if `WIFI_SSID` was set.
pub fn credentials() -> Option<(&'static str, &'static str)> {
    let ssid = option_env!("WIFI_SSID").filter(|s| !s.is_empty())?;
    Some((ssid, option_env!("WIFI_PASSWORD").unwrap_or("")))
}

/// Station configuration for `ssid`; an empty `password` selects an open network.
pub fn station_config(ssid: &str, password: &str) -> Result<WifiConfig, WifiError> {
    let auth = if password.is_empty() {
        AuthenticationMethodConfig::Open
    } else {
        AuthenticationMethodConfig::Wpa2Personal(
            password
                .try_into()
                .map_err(|_| WifiError::InvalidArguments)?,
        )
    };
    let station = StationConfig::default()
        .with_ssid(ssid.try_into().map_err(|_| WifiError::InvalidArguments)?)
        .with_authentication(auth);
    Ok(WifiConfig::Station(station))
}

/// Creates the station interface and starts the Wi-Fi controller with `config`
/// (or an unconfigured station when `None`), following the esp-radio examples.
pub fn init_wifi(
    wifi: WIFI<'static>,
    config: Option<WifiConfig>,
) -> Result<(WifiController<'static>, Interface), WifiError> {
    let interface = Interface::station();
    let controller_config = match config {
        Some(config) => ControllerConfig::default().with_initial_config(config),
        None => ControllerConfig::default(),
    };
    let controller = WifiController::new(wifi, controller_config)?;
    Ok((controller, interface))
}

/// Starts the network stack with DHCP and spawns the runner and the
/// connect/reconnect tasks.
pub fn start(
    spawner: &Spawner,
    controller: WifiController<'static>,
    interface: Interface,
    seed: u64,
) -> Stack<'static> {
    static RESOURCES: StaticCell<StackResources<SOCKETS>> = StaticCell::new();

    let (stack, runner) = embassy_net::new(
        interface,
        NetConfig::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        seed,
    );

    spawner.spawn(net_task(runner).expect("spawn net task"));
    spawner.spawn(connection_task(controller).expect("spawn wifi task"));
    stack
}

/// Waits until DHCP has given the stack an address and returns it.
pub async fn wait_for_address(stack: Stack<'_>) -> embassy_net::Ipv4Cidr {
    loop {
        if let Some(config) = stack.config_v4() {
            return config.address;
        }
        Timer::after(Duration::from_millis(200)).await;
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn connection_task(mut controller: WifiController<'static>) -> ! {
    loop {
        if controller.is_connected() {
            match controller.wait_for_disconnect_async().await {
                Ok(info) => warn!("Wi-Fi disconnected: {:?}", info.reason),
                Err(e) => warn!("Wi-Fi disconnect wait failed: {e:?}"),
            }
            Timer::after(RECONNECT_DELAY).await;
        }

        info!("Wi-Fi connecting...");
        match with_timeout(CONNECT_TIMEOUT, controller.connect_async()).await {
            Ok(Ok(ap)) => info!(
                "Wi-Fi connected to '{}' (channel {}, RSSI {} dBm)",
                ap.ssid.as_str(),
                ap.channel,
                controller.rssi().unwrap_or(0)
            ),
            Ok(Err(e)) => {
                warn!("Wi-Fi connect failed: {e:?}, retrying in 5 s");
                Timer::after(RECONNECT_DELAY).await;
            }
            Err(_) => {
                warn!(
                    "Wi-Fi connect got no answer within {} s, aborting and retrying",
                    CONNECT_TIMEOUT.as_secs()
                );
                // Abort the pending attempt so the next one starts cleanly.
                if let Ok(Err(e)) =
                    with_timeout(RECONNECT_DELAY, controller.disconnect_async()).await
                {
                    warn!("Wi-Fi abort failed: {e:?}");
                }
                Timer::after(RECONNECT_DELAY).await;
            }
        }
    }
}

#[derive(Debug)]
pub enum NtpError {
    Dns,
    Socket,
    Timeout,
    BadReply,
}

/// Offset between the NTP epoch (1900) and the Unix epoch (1970) in seconds.
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

/// Asks `server` for the time (SNTP v4) and returns Unix seconds.
pub async fn ntp_time(stack: Stack<'_>, server: &str) -> Result<u64, NtpError> {
    let addrs = stack
        .dns_query(server, DnsQueryType::A)
        .await
        .map_err(|_| NtpError::Dns)?;
    let addr = *addrs.first().ok_or(NtpError::Dns)?;

    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx_buf = [0u8; 128];
    let mut tx_meta = [PacketMetadata::EMPTY; 2];
    let mut tx_buf = [0u8; 128];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    socket.bind(0).map_err(|_| NtpError::Socket)?;

    // LI = 0, version 4, mode 3 (client).
    let mut request = [0u8; 48];
    request[0] = 0x23;
    socket
        .send_to(&request, (addr, 123))
        .await
        .map_err(|_| NtpError::Socket)?;

    let mut reply = [0u8; 48];
    let (len, _) = with_timeout(Duration::from_secs(5), socket.recv_from(&mut reply))
        .await
        .map_err(|_| NtpError::Timeout)?
        .map_err(|_| NtpError::Socket)?;
    // Mode 4 (server) and a non-zero transmit timestamp.
    if len < 48 || reply[0] & 0x07 != 4 {
        return Err(NtpError::BadReply);
    }
    let seconds = u32::from_be_bytes([reply[40], reply[41], reply[42], reply[43]]);
    u64::from(seconds)
        .checked_sub(NTP_UNIX_OFFSET)
        .filter(|&s| s > 0)
        .ok_or(NtpError::BadReply)
}

/// Calendar date and time (UTC, or local time from [`crate::clock`]).
#[derive(Debug, Clone, Copy)]
pub struct DateTime {
    pub year: i32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl DateTime {
    /// Converts Unix seconds to a date (Howard Hinnant's `civil_from_days`).
    pub fn from_unix(unix: u64) -> Self {
        let days = (unix / 86_400) as i64;
        let secs = unix % 86_400;

        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
        let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;

        Self {
            year,
            month,
            day,
            hour: (secs / 3600) as u8,
            minute: (secs / 60 % 60) as u8,
            second: (secs % 60) as u8,
        }
    }
}

impl core::fmt::Display for DateTime {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}
