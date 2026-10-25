//! The ESP's own IPv4 presence: a DHCP lease from the uplink (or a fallback
//! address on the AP when the uplink is down), `repeater.local` over mDNS and,
//! in fallback, a small DHCP server for the clients.

use crate::status::{self, Addressing};
use bridge_core::{Ipv4Bytes, find_dhcp_option};
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicU32, Ordering};
use embassy_futures::select::{Either, select};
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{ConfigV4, DhcpConfig, IpAddress, IpEndpoint, Ipv4Cidr, Stack, StaticConfigV4};
use embassy_time::{Duration, Timer};
use log::{info, warn};

pub const HOSTNAME: &str = "repeater";
/// The ESP's address on the AP while the uplink is down.
const FALLBACK_ESP_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 1);
/// Last byte of the first address the fallback DHCP server hands out.
const FALLBACK_FIRST_CLIENT_IP: u8 = 100;
const FALLBACK_MAX_CLIENTS: usize = 16;
/// Short, so clients move back to the uplink's DHCP soon after it returns.
const FALLBACK_LEASE_TIME_S: u32 = 60;
/// How long the uplink must be down before switching to the fallback address.
const UPLINK_DOWN_TIME_BEFORE_FALLBACK: Duration = Duration::from_secs(30);

const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_PORT: u16 = 5353;
const DNS_TYPE_A: u16 = 1;
const DNS_TYPE_ANY: u16 = 255;
const DNS_CLASS_INTERNET: u16 = 1;
/// In a question: "unicast reply wanted". In an answer: "flush older records".
const DNS_CLASS_TOP_BIT: u16 = 0x8000;
const MDNS_RECORD_TTL_S: u32 = 120;

const DHCP_SERVER_PORT: u16 = 67;
const DHCP_CLIENT_PORT: u16 = 68;
const DHCP_MAGIC_COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
const BOOTP_REQUEST: u8 = 1;
const BOOTP_REPLY: u8 = 2;
const DHCP_DISCOVER: u8 = 1;
const DHCP_OFFER: u8 = 2;
const DHCP_REQUEST: u8 = 3;
const DHCP_ACK: u8 = 5;
const DHCP_NAK: u8 = 6;
const DHCP_OPTION_SUBNET_MASK: u8 = 1;
const DHCP_OPTION_REQUESTED_IP: u8 = 50;
const DHCP_OPTION_LEASE_TIME: u8 = 51;
const DHCP_OPTION_MESSAGE_TYPE: u8 = 53;
const DHCP_OPTION_SERVER_ID: u8 = 54;
const DHCP_OPTION_END: u8 = 255;

/// The ESP's own address, for the forwarding loop (0 = none yet).
static ESP_IP: AtomicU32 = AtomicU32::new(0);

pub fn esp_ip() -> Option<Ipv4Bytes> {
    let ip = ESP_IP.load(Ordering::Relaxed);
    (ip != 0).then(|| ip.to_be_bytes())
}

fn publish_esp_ip(ip: Option<Ipv4Addr>, addressing: Addressing) {
    ESP_IP.store(ip.map_or(0, |ip| ip.to_bits()), Ordering::Relaxed);
    status::with(|status| status.addressing = addressing);
}

pub fn dhcp_client_config() -> ConfigV4 {
    let mut config = DhcpConfig::default();
    config.hostname = Some(HOSTNAME.try_into().unwrap());
    ConfigV4::Dhcp(config)
}

#[embassy_executor::task]
pub async fn run_ip_stack(mut runner: embassy_net::Runner<'static, crate::netdev::EspNetworkDevice>) -> ! {
    runner.run().await
}

/// Keep a DHCP lease while the uplink is up; fall back to a fixed address on
/// the AP when it has been down for a while.
#[embassy_executor::task]
#[allow(clippy::large_stack_frames, reason = "async state lives in the task arena")]
pub async fn keep_an_address(stack: Stack<'static>) {
    loop {
        stack.set_config_v4(dhcp_client_config());
        publish_esp_ip(None, Addressing::WaitingForLease);
        let mut uplink_down_for = Duration::from_secs(0);
        loop {
            if let Some(lease) = stack.config_v4() {
                info!("Leased {} (gateway {:?})", lease.address, lease.gateway);
                publish_esp_ip(Some(lease.address.address()), Addressing::Leased);
                // Reaching the uplink proves a freshly installed update works.
                crate::ota::confirm_update().await;
                stack.wait_config_down().await;
                warn!("Lease lost");
                publish_esp_ip(None, Addressing::WaitingForLease);
                uplink_down_for = Duration::from_secs(0);
                continue;
            }
            let check_interval = Duration::from_secs(1);
            if let Either::First(()) = select(stack.wait_config_up(), Timer::after(check_interval)).await {
                continue;
            }
            // Never serve DHCP while the uplink's own server is reachable.
            uplink_down_for =
                if is_uplink_connected() { Duration::from_secs(0) } else { uplink_down_for + check_interval };
            if uplink_down_for >= UPLINK_DOWN_TIME_BEFORE_FALLBACK {
                break;
            }
        }

        warn!("Uplink down: falling back to {FALLBACK_ESP_IP} on the AP");
        stack.set_config_v4(ConfigV4::Static(StaticConfigV4 {
            address: Ipv4Cidr::new(FALLBACK_ESP_IP, 24),
            gateway: None,
            dns_servers: heapless::Vec::new(),
        }));
        publish_esp_ip(Some(FALLBACK_ESP_IP), Addressing::Fallback);
        while !is_uplink_connected() {
            Timer::after(Duration::from_secs(1)).await;
        }
        info!("Uplink back: leaving the fallback address");
    }
}

fn is_uplink_connected() -> bool {
    status::with(|status| status.uplink.connected)
}

/// Answer mDNS queries for `repeater.local`, and announce it when the address changes.
#[embassy_executor::task]
#[allow(clippy::large_stack_frames, reason = "async state lives in the task arena")]
pub async fn answer_mdns(stack: Stack<'static>) {
    let mut receive_metadata = [PacketMetadata::EMPTY; 4];
    let mut send_metadata = [PacketMetadata::EMPTY; 4];
    let mut receive_buffer = [0; 1024];
    let mut send_buffer = [0; 512];
    let mut socket =
        UdpSocket::new(stack, &mut receive_metadata, &mut receive_buffer, &mut send_metadata, &mut send_buffer);
    socket.bind(MDNS_PORT).expect("mDNS port taken");
    if let Err(error) = stack.join_multicast_group(MDNS_GROUP) {
        warn!("mDNS: joining the group failed: {error:?}");
    }

    let everyone = IpEndpoint::new(IpAddress::Ipv4(MDNS_GROUP), MDNS_PORT);
    let mut announced_ip: Option<Ipv4Addr> = None;
    let mut query = [0; 512];
    let mut answer = [0; 128];
    loop {
        let esp_ip = stack.config_v4().map(|config| config.address.address());
        if esp_ip != announced_ip
            && let Some(ip) = esp_ip
        {
            info!("mDNS: {HOSTNAME}.local is {ip}");
            let answer_len = write_mdns_answer(&mut answer, 0, ip);
            for _ in 0..2 {
                let _ = socket.send_to(&answer[..answer_len], everyone).await;
                Timer::after(Duration::from_secs(1)).await;
            }
        }
        announced_ip = esp_ip;

        let received = select(socket.recv_from(&mut query), Timer::after(Duration::from_secs(2))).await;
        let (Either::First(Ok((query_len, sender))), Some(ip)) = (received, esp_ip) else { continue };
        let Some(wants_unicast_reply) = asks_for_our_address(&query[..query_len]) else { continue };
        // Queriers on another port than 5353 are simple resolvers: they expect
        // a direct reply that echoes their query ID.
        let is_simple_resolver = sender.endpoint.port != MDNS_PORT;
        let query_id = if is_simple_resolver { u16::from_be_bytes([query[0], query[1]]) } else { 0 };
        let answer_len = write_mdns_answer(&mut answer, query_id, ip);
        let destination = if is_simple_resolver || wants_unicast_reply { sender.endpoint } else { everyone };
        let _ = socket.send_to(&answer[..answer_len], destination).await;
    }
}

/// `Some(wants_unicast_reply)` if the mDNS query asks for our A record.
fn asks_for_our_address(message: &[u8]) -> Option<bool> {
    if message.len() < 12 {
        return None;
    }
    let is_response = message[2] & 0x80 != 0;
    if is_response {
        return None;
    }
    let question_count = u16::from_be_bytes([message[4], message[5]]);
    let mut offset = 12;
    for _ in 0..question_count {
        let (is_our_name, name_end) = read_name_and_compare_with_ours(message, offset)?;
        let record_type = u16::from_be_bytes([*message.get(name_end)?, *message.get(name_end + 1)?]);
        let record_class = u16::from_be_bytes([*message.get(name_end + 2)?, *message.get(name_end + 3)?]);
        offset = name_end + 4;
        let asks_for_address = record_type == DNS_TYPE_A || record_type == DNS_TYPE_ANY;
        if is_our_name && asks_for_address && record_class & !DNS_CLASS_TOP_BIT == DNS_CLASS_INTERNET {
            return Some(record_class & DNS_CLASS_TOP_BIT != 0);
        }
    }
    None
}

/// Compare the DNS name at `offset` with `repeater.local`. Returns the result
/// and the offset just past the name. Follows compression pointers.
fn read_name_and_compare_with_ours(message: &[u8], mut offset: usize) -> Option<(bool, usize)> {
    let mut our_labels = [HOSTNAME, "local"].into_iter();
    let mut matches = true;
    let mut end_of_name = None;
    for _ in 0..16 {
        let label_len = usize::from(*message.get(offset)?);
        let is_pointer = label_len & 0xc0 == 0xc0;
        if is_pointer {
            let pointer_target = (label_len & 0x3f) << 8 | usize::from(*message.get(offset + 1)?);
            end_of_name.get_or_insert(offset + 2);
            offset = pointer_target;
            continue;
        }
        if label_len == 0 {
            let end_of_name = end_of_name.unwrap_or(offset + 1);
            return Some((matches && our_labels.next().is_none(), end_of_name));
        }
        let label = message.get(offset + 1..offset + 1 + label_len)?;
        matches &= our_labels.next().is_some_and(|ours| ours.as_bytes().eq_ignore_ascii_case(label));
        offset += 1 + label_len;
    }
    None
}

/// Write an mDNS response holding our A record; return its length.
fn write_mdns_answer(buffer: &mut [u8], query_id: u16, ip: Ipv4Addr) -> usize {
    let mut out = ByteWriter { buffer, len: 0 };
    out.append(&query_id.to_be_bytes());
    out.append(&[0x84, 0x00]); // Flags: authoritative response.
    out.append(&[0, 0, 0, 1, 0, 0, 0, 0]); // 0 questions, 1 answer, 0 authority, 0 additional.
    out.append(&[HOSTNAME.len() as u8]);
    out.append(HOSTNAME.as_bytes());
    out.append(&[5]);
    out.append(b"local");
    out.append(&[0]);
    out.append(&DNS_TYPE_A.to_be_bytes());
    out.append(&(DNS_CLASS_INTERNET | DNS_CLASS_TOP_BIT).to_be_bytes());
    out.append(&MDNS_RECORD_TTL_S.to_be_bytes());
    out.append(&4u16.to_be_bytes());
    out.append(&ip.octets());
    out.len
}

struct ByteWriter<'a> {
    buffer: &'a mut [u8],
    len: usize,
}

impl ByteWriter<'_> {
    fn append(&mut self, bytes: &[u8]) {
        self.buffer[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }
}

/// In fallback mode, lease addresses on 192.168.4.0/24 to the clients.
#[embassy_executor::task]
#[allow(clippy::large_stack_frames, reason = "async state lives in the task arena")]
pub async fn serve_fallback_dhcp(stack: Stack<'static>) {
    let mut receive_metadata = [PacketMetadata::EMPTY; 2];
    let mut send_metadata = [PacketMetadata::EMPTY; 2];
    let mut receive_buffer = [0; 1024];
    let mut send_buffer = [0; 512];
    let mut socket =
        UdpSocket::new(stack, &mut receive_metadata, &mut receive_buffer, &mut send_metadata, &mut send_buffer);
    socket.bind(DHCP_SERVER_PORT).expect("DHCP server port taken");

    // Client MAC for each address in the pool, reused round-robin.
    let mut client_macs = [[0u8; 6]; FALLBACK_MAX_CLIENTS];
    let mut next_free_slot = 0;
    let mut request_buffer = [0; 576];
    let mut reply = [0; 300];
    loop {
        let Ok((request_len, _)) = socket.recv_from(&mut request_buffer).await else { continue };
        if status::with(|status| status.addressing) != Addressing::Fallback {
            continue;
        }
        let request = &request_buffer[..request_len];
        if request.len() < 240 || request[0] != BOOTP_REQUEST || request[236..240] != DHCP_MAGIC_COOKIE {
            continue;
        }
        let client_mac: [u8; 6] = request[28..34].try_into().unwrap();
        let slot = client_macs.iter().position(|mac| *mac == client_mac).unwrap_or_else(|| {
            let slot = next_free_slot;
            next_free_slot = (next_free_slot + 1) % FALLBACK_MAX_CLIENTS;
            client_macs[slot] = client_mac;
            slot
        });
        let offered_ip = Ipv4Addr::new(192, 168, 4, FALLBACK_FIRST_CLIENT_IP + slot as u8);
        let options = &request[240..];
        let request_type = find_dhcp_option(options, DHCP_OPTION_MESSAGE_TYPE).and_then(|value| value.first().copied());
        let reply_type = match request_type {
            Some(DHCP_DISCOVER) => DHCP_OFFER,
            Some(DHCP_REQUEST) => {
                // NAK a request for another address (e.g. its old lease from
                // the uplink), so the client starts over.
                let client_current_ip = Ipv4Addr::new(request[12], request[13], request[14], request[15]);
                let requested_ip = find_dhcp_option(options, DHCP_OPTION_REQUESTED_IP)
                    .and_then(|value| <[u8; 4]>::try_from(value).ok())
                    .map(Ipv4Addr::from)
                    .unwrap_or(client_current_ip);
                if requested_ip == offered_ip { DHCP_ACK } else { DHCP_NAK }
            }
            _ => continue,
        };
        let reply_len = write_dhcp_reply(&mut reply, request, reply_type, offered_ip);
        let outcome = if reply_type == DHCP_NAK { "NAK" } else { "lease" };
        info!("Fallback DHCP: {outcome} {offered_ip} to {client_mac:02x?}");
        let everyone = IpEndpoint::new(IpAddress::Ipv4(Ipv4Addr::BROADCAST), DHCP_CLIENT_PORT);
        let _ = socket.send_to(&reply[..reply_len], everyone).await;
    }
}

fn write_dhcp_reply(buffer: &mut [u8], request: &[u8], message_type: u8, offered_ip: Ipv4Addr) -> usize {
    let mut out = ByteWriter { buffer, len: 0 };
    out.append(&[BOOTP_REPLY, 1, 6, 0]); // Ethernet, 6-byte MAC, 0 hops.
    out.append(&request[4..8]); // Transaction ID.
    out.append(&[0, 0]); // Seconds elapsed.
    out.append(&request[10..12]); // Flags.
    out.append(&[0; 4]); // Client's current IP.
    out.append(&if message_type == DHCP_NAK { [0; 4] } else { offered_ip.octets() });
    out.append(&[0; 4]); // Next server IP.
    out.append(&request[24..28]); // Relay agent IP.
    out.append(&request[28..44]); // Client hardware address.
    out.append(&[0; 192]); // Server name and boot file.
    out.append(&DHCP_MAGIC_COOKIE);
    out.append(&[DHCP_OPTION_MESSAGE_TYPE, 1, message_type]);
    out.append(&[DHCP_OPTION_SERVER_ID, 4]);
    out.append(&FALLBACK_ESP_IP.octets());
    if message_type != DHCP_NAK {
        out.append(&[DHCP_OPTION_LEASE_TIME, 4]);
        out.append(&FALLBACK_LEASE_TIME_S.to_be_bytes());
        out.append(&[DHCP_OPTION_SUBNET_MASK, 4, 255, 255, 255, 0]);
    }
    out.append(&[DHCP_OPTION_END]);
    out.len
}
