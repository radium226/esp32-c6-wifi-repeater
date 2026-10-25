//! Same-subnet Wi-Fi repeater for the Seeed XIAO ESP32C6.
//!
//! Joins the home Wi-Fi as a station (the uplink), opens its own AP on the
//! same channel and bridges IPv4 between the two with `bridge_core::Bridge`,
//! so clients get addresses from the home router. The ESP also has its own IP
//! stack ("the ESP") that serves a status page at `http://repeater.local/`,
//! which also takes OTA updates.
#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

mod net;
mod netdev;
mod ota;
mod status;
mod web;

use bridge_core::{Bridge, MacAddress, UplinkFrameAction};
use core::future::poll_fn;
use core::task::{Context, Poll};
use embassy_executor::Spawner;
use embassy_futures::select::{Either3, select, select3};
use embassy_net::StackResources;
use embassy_net_driver::{Driver, RxToken, TxToken};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::rng::Rng;
use esp_hal::rtc_cntl::{Rtc, RwdtStage};
use esp_hal::timer::timg::TimerGroup;
use esp_hal::tsens::TemperatureSensor;
use esp_radio::wifi::ap::AccessPointConfig;
use esp_radio::wifi::event::EventInfo;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{
    AuthenticationMethodConfig, Bandwidth, Config, ControllerConfig, Interface, PowerSaveMode, WifiController,
};
use log::{info, warn};
use static_cell::StaticCell;

extern crate alloc;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

/// Use the U.FL connector instead of the on-board ceramic antenna.
const EXTERNAL_ANTENNA: bool = false;
/// 15 dBm, in the driver's units of 0.25 dBm.
const TX_POWER_QUARTER_DBM: i8 = 60;
const WATCHDOG_TIMEOUT_S: u64 = 10;

/// MACs of clients that left the AP, for the forwarding loop to forget.
static CLIENTS_THAT_LEFT: Channel<CriticalSectionRawMutex, MacAddress, 4> = Channel::new();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();

    let hal_config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(hal_config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 65536);
    // The Wi-Fi driver allocates up to 64 TX and 64 RX buffers (~1.6 KB each)
    // from the heap; with only 64 KB, TX failed with ESP_ERR_NO_MEM under load.
    esp_alloc::heap_allocator!(size: 224 * 1024);

    // Roll back an unconfirmed update before starting anything else.
    ota::check_at_boot(esp_storage::FlashStorage::new(peripherals.FLASH));
    // Reset on a panic or a hang, so a bad update rolls back by itself.
    let mut rtc = Rtc::new(peripherals.RTC_TIMER);
    rtc.rwdt.set_timeout(RwdtStage::Stage0, esp_hal::time::Duration::from_secs(WATCHDOG_TIMEOUT_S));
    rtc.rwdt.enable();

    let timer_group = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timer_group.timer0, peripherals.FROM_CPU_INTR0);

    // XIAO ESP32C6 RF switch: GPIO3 low powers it, GPIO14 picks the antenna.
    let _rf_switch_power = Output::new(peripherals.GPIO3, Level::Low, OutputConfig::default());
    let _antenna_select = Output::new(
        peripherals.GPIO14,
        if EXTERNAL_ANTENNA { Level::High } else { Level::Low },
        OutputConfig::default(),
    );

    let station_config = StationConfig::default()
        .with_ssid(env!("STA_SSID").try_into().expect("STA_SSID too long"))
        .with_authentication(AuthenticationMethodConfig::Wpa2Personal(
            env!("STA_PASS").try_into().expect("STA_PASS too long"),
        ));
    // The channel follows the station once it connects (single radio).
    let access_point_config = AccessPointConfig::default()
        .with_ssid(env!("AP_SSID").try_into().expect("AP_SSID too long"))
        .with_authentication(AuthenticationMethodConfig::Wpa2Personal(
            env!("AP_PASS").try_into().expect("AP_PASS too long"),
        ));
    let controller_config = ControllerConfig::default()
        .with_country_info(*b"FR")
        // The defaults (5 RX, 3 TX) drop TCP bursts while frames cross the bridge.
        // The TX queue caps frames in flight on both interfaces together, and
        // receiving stops while it is full.
        .with_rx_queue_size(32)
        .with_tx_queue_size(64)
        .with_static_rx_buf_num(16)
        .with_dynamic_rx_buf_num(64)
        .with_dynamic_tx_buf_num(64)
        .with_rx_ba_win(16)
        .with_initial_config(Config::AccessPointStation(station_config, access_point_config));
    let mut controller = WifiController::new(peripherals.WIFI, controller_config)
        .expect("Failed to initialize Wi-Fi controller");
    // Modem sleep makes the station miss frames for the clients.
    controller
        .set_power_saving(PowerSaveMode::None)
        .expect("Failed to disable power saving");
    // esp-radio defaults to 5 dBm, too quiet for the Freebox to hear our auth
    // requests. Above ~16 dBm some boards fail auth, so stay at 15 dBm.
    controller.set_max_tx_power(TX_POWER_QUARTER_DBM).expect("Failed to set TX power");
    use_20_mhz(&mut controller);

    let station = Interface::station();
    let access_point = Interface::access_point();
    info!("station MAC {:02x?}, AP MAC {:02x?}", station.mac_address(), access_point.mac_address());

    // The ESP's own IP stack answers as the station MAC on both sides.
    static IP_STACK_RESOURCES: StaticCell<StackResources<6>> = StaticCell::new();
    static BOOT_INFO: StaticCell<web::BootInfo> = StaticCell::new();
    let random = Rng::new();
    let random_seed = u64::from(random.random()) << 32 | u64::from(random.random());
    let mut ip_config = embassy_net::Config::default();
    ip_config.ipv4 = net::dhcp_client_config();
    let (esp_device, esp_queues) = netdev::new(station.mac_address());
    let (stack, ip_stack_runner) =
        embassy_net::new(esp_device, ip_config, IP_STACK_RESOURCES.init(StackResources::new()), random_seed);
    let boot_info = BOOT_INFO.init(web::BootInfo {
        station_mac: station.mac_address(),
        access_point_mac: access_point.mac_address(),
    });
    let temperature_sensor =
        TemperatureSensor::new(peripherals.TSENS, Default::default()).expect("No temperature sensor");

    spawner.spawn(feed_watchdog(rtc).expect("watchdog task already spawned"));
    spawner.spawn(ota::roll_back_unconfirmed_trial().expect("trial task already spawned"));
    spawner.spawn(keep_uplink_connected(controller, temperature_sensor).expect("uplink task already spawned"));
    spawner.spawn(net::run_ip_stack(ip_stack_runner).expect("IP stack task already spawned"));
    spawner.spawn(net::keep_an_address(stack).expect("addressing task already spawned"));
    spawner.spawn(net::answer_mdns(stack).expect("mDNS task already spawned"));
    spawner.spawn(net::serve_fallback_dhcp(stack).expect("DHCP server task already spawned"));
    for _ in 0..2 {
        spawner.spawn(web::serve_http(stack, boot_info).expect("web tasks already spawned"));
    }
    forward_frames(station, access_point, esp_queues).await
}

/// Feed the watchdog. If the executor stalls (a task never yields, or a
/// panic halts the chip), the watchdog resets it.
#[embassy_executor::task]
async fn feed_watchdog(mut rtc: Rtc<'static>) {
    loop {
        rtc.rwdt.feed();
        Timer::after(Duration::from_secs(2)).await;
    }
}

/// Keep the station connected, track the AP's clients, and refresh the
/// uplink's signal and the chip temperature for the status page.
#[embassy_executor::task]
#[allow(clippy::large_stack_frames, reason = "async state lives in the task arena")]
async fn keep_uplink_connected(mut controller: WifiController<'static>, temperature_sensor: TemperatureSensor<'static>) {
    let mut failed_attempts_in_a_row = 0;
    loop {
        info!("Connecting to {}", env!("STA_SSID"));
        match controller.connect_async().await {
            Ok(connection) => {
                info!("Uplink up on channel {}", connection.channel);
                failed_attempts_in_a_row = 0;
                // Following the uplink's channel can put the AP back on 40 MHz.
                use_20_mhz(&mut controller);
                let bssid = controller.ap_info().map(|uplink_ap| uplink_ap.bssid).unwrap_or_default();
                status::with(|status| {
                    status.uplink.connected = true;
                    status.uplink.changed_at_s = uptime_s();
                    status.uplink.bssid = bssid;
                    status.uplink.channel = connection.channel;
                });
            }
            Err(error) => {
                warn!("Uplink connection failed: {error:?}");
                status::with(|status| status.uplink.failed_attempts += 1);
                failed_attempts_in_a_row += 1;
                // Keep tracking clients while waiting to retry. The event
                // subscription borrows the controller, so events during the
                // connection attempt itself are missed.
                let mut wifi_events = controller.subscribe().expect("No event subscriber left");
                // Each attempt scans every channel, taking the AP off its
                // channel; back off so the fallback page stays usable.
                let retry_delay_s = if failed_attempts_in_a_row < 3 { 5 } else { 30 };
                let retry_timer = Timer::after(Duration::from_secs(retry_delay_s));
                let track_clients = async {
                    loop {
                        track_client(wifi_events.next_event_pure().await);
                    }
                };
                select(track_clients, retry_timer).await;
                continue;
            }
        }

        let mut wifi_events = controller.subscribe().expect("No event subscriber left");
        loop {
            let refresh_timer = Timer::after(Duration::from_secs(2));
            match select3(controller.wait_for_disconnect_async(), wifi_events.next_event_pure(), refresh_timer).await {
                Either3::First(disconnection) => {
                    warn!("Uplink lost: {disconnection:?}");
                    break;
                }
                Either3::Second(event) => track_client(event),
                Either3::Third(()) => {
                    let signal_dbm = controller.rssi().unwrap_or(0);
                    let chip_temperature_c = temperature_sensor.get_temperature().to_celsius();
                    status::with(|status| {
                        status.uplink.signal_dbm = signal_dbm.clamp(-128, 0) as i8;
                        status.chip_temperature_c = chip_temperature_c;
                    });
                }
            }
        }
        drop(wifi_events);
        status::with(|status| {
            status.uplink.connected = false;
            status.uplink.changed_at_s = uptime_s();
        });
        Timer::after(Duration::from_secs(1)).await;
    }
}

/// Track clients joining and leaving the AP.
fn track_client(event: EventInfo) {
    match event {
        EventInfo::AccessPointStationConnected { mac, .. } => {
            info!("Client joined: {mac:02x?}");
            status::with(|status| {
                status.associated_clients.retain(|client| client.mac != mac);
                let _ = status.associated_clients.push(status::AssociatedClient { mac, joined_at_s: uptime_s() });
            });
        }
        EventInfo::AccessPointStationDisconnected { mac, .. } => {
            info!("Client left: {mac:02x?}");
            status::with(|status| status.associated_clients.retain(|client| client.mac != mac));
            if CLIENTS_THAT_LEFT.try_send(mac).is_err() {
                warn!("Client-left queue full, {mac:02x?} will expire instead");
            }
        }
        _ => {}
    }
}

fn uptime_s() -> u32 {
    Instant::now().as_secs() as u32
}

/// The Freebox's 2.4 GHz radio is 20 MHz only; a 40 MHz AP on the same radio
/// slowed the bridge down.
fn use_20_mhz(controller: &mut WifiController<'_>) {
    let result = controller
        .bandwidths()
        .and_then(|bandwidths| controller.set_bandwidths(bandwidths.with_2_4(Bandwidth::_20MHz)));
    match (result, controller.bandwidths(), controller.channel()) {
        (Ok(()), Ok(bandwidths), Ok(channel)) => info!("AP bandwidth {bandwidths:?}, channel {channel:?}"),
        (result, ..) => warn!("Setting 20 MHz failed: {result:?}"),
    }
}

/// Move frames between the uplink, the clients and the ESP until the end of time.
#[allow(clippy::large_stack_frames, reason = "async state lives in the task arena")]
async fn forward_frames(mut station: Interface, mut access_point: Interface, mut esp_queues: netdev::EspQueues) -> ! {
    let mut bridge = Bridge::new(station.mac_address(), access_point.mac_address());
    let mut esp_ip = None;
    let mut last_client_log = Instant::now();
    let mut last_stats_log = Instant::now();
    let mut last_status_update = Instant::now();
    let mut stats = Stats::default();
    let mut traffic = status::Traffic::default();
    loop {
        poll_fn(|context| {
            stats.wakeups += 1;
            while let Ok(mac) = CLIENTS_THAT_LEFT.try_receive() {
                bridge.forget_client(&mac);
            }
            if net::esp_ip() != esp_ip {
                esp_ip = net::esp_ip();
                bridge.set_esp_ip(esp_ip);
            }
            let now_s = uptime_s();
            let mut did_work = false;

            if let Some((received, _)) = Driver::receive(&mut station, context) {
                did_work = true;
                stats.frames_from_uplink += 1;
                received.consume(|frame| {
                    traffic.frames_from_uplink += 1;
                    traffic.bytes_from_uplink += frame.len() as u64;
                    let sent = match bridge.handle_frame_from_uplink(frame, now_s) {
                        UplinkFrameAction::Drop => true,
                        UplinkFrameAction::SendToClients => try_send(&mut access_point, context, frame),
                        UplinkFrameAction::ReplyToUplink => try_send(&mut station, context, frame),
                        UplinkFrameAction::SendToEsp => esp_queues.send_to_esp(frame),
                        UplinkFrameAction::SendToClientsAndEsp => {
                            esp_queues.send_to_esp(frame) & try_send(&mut access_point, context, frame)
                        }
                    };
                    stats.dropped_frames += !sent as u32;
                });
            }

            if let Some((received, _)) = Driver::receive(&mut access_point, context) {
                did_work = true;
                stats.frames_from_clients += 1;
                received.consume(|frame| {
                    traffic.frames_from_clients += 1;
                    traffic.bytes_from_clients += frame.len() as u64;
                    let action = bridge.handle_frame_from_client(frame, now_s);
                    if action.send_to_esp {
                        stats.dropped_frames += !esp_queues.send_to_esp(frame) as u32;
                    }
                    if action.send_to_other_clients {
                        stats.dropped_frames += !try_send(&mut access_point, context, frame) as u32;
                    }
                    if action.send_to_uplink {
                        bridge.rewrite_client_frame_for_uplink(frame);
                        stats.dropped_frames += !try_send(&mut station, context, frame) as u32;
                    }
                });
            }

            did_work |= esp_queues.handle_frame_from_esp(context, |frame, context| {
                traffic.frames_from_esp += 1;
                let destinations = bridge.route_frame_from_esp(frame);
                if destinations.send_to_clients {
                    stats.dropped_frames += !try_send(&mut access_point, context, frame) as u32;
                }
                if destinations.send_to_uplink {
                    stats.dropped_frames += !try_send(&mut station, context, frame) as u32;
                }
            });

            // Return after each batch so the other tasks can run.
            if did_work {
                Poll::Ready(())
            } else {
                stats.idle_wakeups += 1;
                Poll::Pending
            }
        })
        .await;

        if last_status_update.elapsed() > Duration::from_secs(1) {
            last_status_update = Instant::now();
            traffic.dropped_frames += stats.dropped_frames - stats.dropped_frames_already_counted;
            stats.dropped_frames_already_counted = stats.dropped_frames;
            let client_addresses = client_addresses_for_status(&bridge);
            status::with(|status| {
                status.traffic = traffic;
                status.client_addresses = client_addresses;
            });
        }

        if last_stats_log.elapsed() > Duration::from_secs(5) {
            last_stats_log = Instant::now();
            if stats.frames_from_uplink + stats.frames_from_clients > 0 {
                info!(
                    "5 s: {} from uplink, {} from clients, {} dropped, {} wakeups ({} idle), {} B heap free",
                    stats.frames_from_uplink,
                    stats.frames_from_clients,
                    stats.dropped_frames,
                    stats.wakeups,
                    stats.idle_wakeups,
                    esp_alloc::HEAP.free()
                );
            }
            stats = Stats::default();
        }

        if last_client_log.elapsed() > Duration::from_secs(60) {
            last_client_log = Instant::now();
            for address in bridge.known_client_addresses(uptime_s()) {
                info!("client {:?} -> {:02x?} ({} s left)", address.ip, address.mac, address.expires_in_s);
            }
        }
    }
}

/// Frame counters for the periodic log, reset every 5 s.
#[derive(Default)]
struct Stats {
    frames_from_uplink: u32,
    frames_from_clients: u32,
    dropped_frames: u32,
    /// Part of `dropped_frames` already added to the status page's total.
    dropped_frames_already_counted: u32,
    wakeups: u32,
    idle_wakeups: u32,
}

#[allow(clippy::large_stack_frames, reason = "a few KB of snapshot; the main stack is 80 KB")]
fn client_addresses_for_status(
    bridge: &Bridge,
) -> heapless::Vec<status::ClientAddress, { bridge_core::MAX_CLIENT_ADDRESSES }> {
    bridge
        .known_client_addresses(uptime_s())
        .map(|address| status::ClientAddress {
            ip: address.ip,
            mac: address.mac,
            expires_in_s: address.expires_in_s,
            hostname: bridge
                .client_hostname(&address.mac)
                .and_then(|hostname| hostname.try_into().ok())
                .unwrap_or_default(),
        })
        .collect()
}

/// Queue a frame on an interface, dropping it if the TX queue is full (like a
/// switch would). Returns whether the frame was queued.
fn try_send(interface: &mut Interface, context: &mut Context<'_>, frame: &[u8]) -> bool {
    match Driver::transmit(interface, context) {
        Some(transmission) => {
            transmission.consume(frame.len(), |buffer| buffer.copy_from_slice(frame));
            true
        }
        None => false,
    }
}
