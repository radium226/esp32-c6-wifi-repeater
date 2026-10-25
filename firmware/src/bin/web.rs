//! Status page on port 80: `/` serves the page, `/api/status` the JSON it polls,
//! and `POST /update` takes a firmware image (see `ota.rs`).

use crate::net::HOSTNAME;
use crate::ota;
use crate::status::{self, Addressing};
use alloc::string::String;
use bridge_core::MacAddress;
use core::fmt::Write as _;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_time::{Duration, Instant};
use embedded_io_async::Write as _;

const PAGE_HTML: &str = include_str!("page.html");
const HTTP_PORT: u16 = 80;
const END_OF_HEADERS: &[u8] = b"\r\n\r\n";

/// Facts fixed at boot, for the page.
pub struct BootInfo {
    pub station_mac: MacAddress,
    pub access_point_mac: MacAddress,
}

#[embassy_executor::task(pool_size = 2)]
#[allow(clippy::large_stack_frames, reason = "async state lives in the task arena")]
pub async fn serve_http(stack: Stack<'static>, boot_info: &'static BootInfo) {
    // A bigger receive window makes uploads faster.
    let mut receive_buffer = [0; 4096];
    let mut send_buffer = [0; 2048];
    let mut request_buffer = [0; 1024];
    let mut sector_buffer = [0; ota::FLASH_SECTOR_SIZE];
    loop {
        let mut socket = TcpSocket::new(stack, &mut receive_buffer, &mut send_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));
        if socket.accept(HTTP_PORT).await.is_err() {
            continue;
        }
        let mut request_len = 0;
        while request_len < request_buffer.len() {
            match socket.read(&mut request_buffer[request_len..]).await {
                Ok(0) | Err(_) => break,
                Ok(bytes_read) => request_len += bytes_read,
            }
            if request_buffer[..request_len].windows(4).any(|window| window == END_OF_HEADERS) {
                break;
            }
        }
        let request = &request_buffer[..request_len];
        let headers_len = request
            .windows(4)
            .position(|window| window == END_OF_HEADERS)
            .map_or(request_len, |position| position + END_OF_HEADERS.len());
        let (headers, body_already_received) = request.split_at(headers_len);
        let mut request_line = headers.split(|&byte| byte == b' ');
        let method = request_line.next().unwrap_or(b"");
        let path = request_line.next().unwrap_or(b"");
        let _ = match (method, path) {
            (b"POST", b"/update") => {
                let result = receive_update(&mut socket, headers, body_already_received, &mut sector_buffer).await;
                let (http_status, message) = match result {
                    Ok(()) => ("200 OK", "Update written, rebooting into it\n"),
                    Err(error) => ("400 Bad Request", error),
                };
                let response_sent = respond(&mut socket, http_status, "text/plain", message.as_bytes()).await;
                if result.is_ok() {
                    socket.close();
                    let _ = socket.flush().await;
                    ota::reboot_soon().await;
                }
                response_sent
            }
            (b"GET", b"/" | b"/index.html") => {
                respond(&mut socket, "200 OK", "text/html; charset=utf-8", PAGE_HTML.as_bytes()).await
            }
            (b"GET", b"/api/status") => {
                let json = status_json(stack, boot_info);
                respond(&mut socket, "200 OK", "application/json", json.as_bytes()).await
            }
            _ => respond(&mut socket, "404 Not Found", "text/plain", b"Not found\n").await,
        };
        socket.close();
        let _ = socket.flush().await;
        socket.abort();
    }
}

/// Stream an upload into the inactive app slot, one flash sector at a time.
#[allow(clippy::large_stack_frames, reason = "async state lives in the task arena")]
async fn receive_update(
    socket: &mut TcpSocket<'_>,
    headers: &[u8],
    body_already_received: &[u8],
    sector_buffer: &mut [u8; ota::FLASH_SECTOR_SIZE],
) -> Result<(), &'static str> {
    if header_value(headers, b"x-ota-password") != Some(env!("OTA_PASS").as_bytes()) {
        return Err("wrong or missing X-OTA-Password\n");
    }
    let image_size = header_value(headers, b"content-length")
        .and_then(|value| core::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or("Content-Length required\n")?;
    let mut upload = ota::Upload::start(image_size)?;
    let mut unread_body = body_already_received;
    let mut bytes_received = 0;
    while bytes_received < image_size {
        // Fill one sector (or the rest of the image).
        let sector_len = (image_size - bytes_received).min(sector_buffer.len());
        let mut sector_filled = 0;
        while sector_filled < sector_len {
            if !unread_body.is_empty() {
                let take = unread_body.len().min(sector_len - sector_filled);
                sector_buffer[sector_filled..sector_filled + take].copy_from_slice(&unread_body[..take]);
                unread_body = &unread_body[take..];
                sector_filled += take;
                continue;
            }
            match socket.read(&mut sector_buffer[sector_filled..sector_len]).await {
                Ok(0) | Err(_) => return Err("upload cut short\n"),
                Ok(bytes_read) => sector_filled += bytes_read,
            }
        }
        upload.write(&sector_buffer[..sector_filled])?;
        bytes_received += sector_filled;
    }
    upload.finish()
}

/// Value of an HTTP header (`lowercase_name` in lowercase), trimmed.
fn header_value<'a>(headers: &'a [u8], lowercase_name: &[u8]) -> Option<&'a [u8]> {
    headers.split(|&byte| byte == b'\n').find_map(|line| {
        let colon = line.iter().position(|&byte| byte == b':')?;
        let (name, colon_and_value) = line.split_at(colon);
        name.eq_ignore_ascii_case(lowercase_name).then(|| colon_and_value[1..].trim_ascii())
    })
}

#[allow(clippy::large_stack_frames, reason = "async state lives in the task arena")]
async fn respond(
    socket: &mut TcpSocket<'_>,
    http_status: &str,
    content_type: &str,
    body: &[u8],
) -> Result<(), embassy_net::tcp::Error> {
    let mut response_headers = String::new();
    let _ = write!(
        response_headers,
        "HTTP/1.1 {http_status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(response_headers.as_bytes()).await?;
    socket.write_all(body).await
}

#[allow(clippy::large_stack_frames, reason = "a few KB of snapshot; the main stack is 80 KB")]
fn status_json(stack: Stack<'static>, boot_info: &BootInfo) -> String {
    let mut json = String::with_capacity(2048);
    let uptime_s = Instant::now().as_secs();
    let ip_config = stack.config_v4();
    let (ota_slot, ota_state) = ota::running_slot_and_state();
    // Copy first: `status::with` runs with interrupts off.
    let status = status::with(|status| status.clone());

    let _ = write!(
        json,
        r#"{{"hostname":"{HOSTNAME}","version":"{}","build_time_unix_s":{},"ota":{{"slot":"{ota_slot}","state":"{ota_state}"}},"uptime_s":{uptime_s},"memory_free_bytes":{},"memory_used_bytes":{},"chip_temperature_c":{:.1},"tx_power_dbm":15,"#,
        env!("CARGO_PKG_VERSION"),
        env!("BUILD_TIME"),
        esp_alloc::HEAP.free(),
        esp_alloc::HEAP.used(),
        status.chip_temperature_c,
    );

    let addressing_mode = match status.addressing {
        Addressing::WaitingForLease => "waiting_for_lease",
        Addressing::Leased => "leased",
        Addressing::Fallback => "fallback",
    };
    let _ = write!(json, r#""ip":{{"mode":"{addressing_mode}","address":"#);
    match &ip_config {
        Some(config) => {
            let address = config.address.address();
            let _ = write!(json, r#""{address}","prefix_len":{},"gateway":"#, config.address.prefix_len());
            match config.gateway {
                Some(gateway) => {
                    let _ = write!(json, r#""{gateway}"}},"#);
                }
                None => json.push_str("null},"),
            }
        }
        None => json.push_str("null},"),
    }

    let uplink = &status.uplink;
    json.push_str(r#""uplink":{"ssid":"#);
    push_json_string(&mut json, env!("STA_SSID"));
    let _ = write!(
        json,
        r#","connected":{},"changed_at_s":{},"bssid":"{}","channel":{},"signal_dbm":{},"failed_attempts":{}}},"#,
        uplink.connected,
        uplink.changed_at_s,
        FormattedMac(&uplink.bssid),
        uplink.channel,
        uplink.signal_dbm,
        uplink.failed_attempts
    );
    json.push_str(r#""access_point":{"ssid":"#);
    push_json_string(&mut json, env!("AP_SSID"));
    let _ = write!(
        json,
        r#","mac":"{}","station_mac":"{}"}},"#,
        FormattedMac(&boot_info.access_point_mac),
        FormattedMac(&boot_info.station_mac)
    );

    let traffic = &status.traffic;
    let _ = write!(
        json,
        r#""traffic":{{"frames_from_uplink":{},"bytes_from_uplink":{},"frames_from_clients":{},"bytes_from_clients":{},"dropped_frames":{},"frames_from_esp":{}}},"#,
        traffic.frames_from_uplink,
        traffic.bytes_from_uplink,
        traffic.frames_from_clients,
        traffic.bytes_from_clients,
        traffic.dropped_frames,
        traffic.frames_from_esp
    );

    json.push_str(r#""associated_clients":["#);
    for (index, client) in status.associated_clients.iter().enumerate() {
        let separator = if index > 0 { "," } else { "" };
        let _ = write!(
            json,
            r#"{separator}{{"mac":"{}","joined_at_s":{}}}"#,
            FormattedMac(&client.mac),
            client.joined_at_s
        );
    }
    json.push_str(r#"],"client_addresses":["#);
    for (index, address) in status.client_addresses.iter().enumerate() {
        let separator = if index > 0 { "," } else { "" };
        let [a, b, c, d] = address.ip;
        let _ = write!(
            json,
            r#"{separator}{{"ip":"{a}.{b}.{c}.{d}","mac":"{}","expires_in_s":{},"hostname":"#,
            FormattedMac(&address.mac),
            address.expires_in_s
        );
        push_json_string(&mut json, &address.hostname);
        json.push('}');
    }
    json.push_str("]}");
    json
}

/// Append `text` as a JSON string literal.
fn push_json_string(json: &mut String, text: &str) {
    json.push('"');
    for character in text.chars() {
        match character {
            '"' => json.push_str("\\\""),
            '\\' => json.push_str("\\\\"),
            control if (control as u32) < 0x20 => {
                let _ = write!(json, "\\u{:04x}", control as u32);
            }
            other => json.push(other),
        }
    }
    json.push('"');
}

/// Shows a MAC as `aa:bb:cc:dd:ee:ff`.
struct FormattedMac<'a>(&'a MacAddress);

impl core::fmt::Display for FormattedMac<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let [a, b, c, d, e, f] = self.0;
        write!(formatter, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}")
    }
}
