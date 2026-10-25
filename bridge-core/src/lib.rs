//! IPv4 pseudo-bridge between a Wi-Fi station (the uplink) and a soft-AP (the
//! clients).
//!
//! A Wi-Fi station may only send frames from its own MAC, so a plain L2 bridge
//! is impossible. Instead every client hides behind the station MAC on the
//! uplink, and frames coming back are steered to the right client by IP:
//!
//! * client -> uplink: source MAC (and ARP sender MAC) become the station MAC;
//!   the client's IP -> MAC is remembered in the client address table.
//! * uplink -> client: frames for the station MAC are sent to the client owning
//!   the destination IP. Uplink MACs are kept as-is (the soft-AP may send frames
//!   with any source address), so clients talk to the real uplink hosts.
//! * Uplink ARP requests for a client IP get a proxy reply with the station MAC.
//! * DHCP requests get the broadcast flag so the offer reaches the client; the
//!   client's hardware address (chaddr) is kept because the Freebox leases by
//!   it. The ACK fills the client address table.
//! * The ESP's own IP stack ("the ESP" below) answers as the station MAC on both
//!   sides. Frames for it (and every broadcast) are handed to it; its own frames
//!   are routed with [`Bridge::route_frame_from_esp`].
//!
//! Only IPv4 and ARP are bridged: IPv6 neighbor discovery would need its own
//! proxy, so clients stay IPv4-only (same as the ESP8266 bridge).
#![cfg_attr(not(test), no_std)]

pub type MacAddress = [u8; 6];
pub type Ipv4Bytes = [u8; 4];

pub const MAX_CLIENT_ADDRESSES: usize = 32;
/// How long a client address is remembered without traffic.
pub const CLIENT_ADDRESS_LIFETIME_S: u32 = 600;
/// Client hostnames remembered from their DHCP requests.
pub const MAX_CLIENT_HOSTNAMES: usize = 16;
pub const MAX_HOSTNAME_LEN: usize = 32;

const ETHERNET_HEADER_LEN: usize = 14;
const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_ARP: u16 = 0x0806;
const ARP_PACKET_LEN: usize = 28;
const ARP_REQUEST: u16 = 1;
const ARP_REPLY: u16 = 2;
const IPV4_MIN_HEADER_LEN: usize = 20;
const IP_PROTOCOL_UDP: u8 = 17;
const DHCP_SERVER_PORT: u16 = 67;
const DHCP_CLIENT_PORT: u16 = 68;
const DHCP_FIXED_PART_LEN: usize = 240; // BOOTP header + magic cookie
const DHCP_MAGIC_COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
const BOOTP_REQUEST: u8 = 1;
const BOOTP_REPLY: u8 = 2;
const DHCP_ACK: u8 = 5;
const DHCP_OPTION_HOSTNAME: u8 = 12;
const DHCP_OPTION_MESSAGE_TYPE: u8 = 53;

/// What to do with a frame received from a client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClientFrameAction {
    /// Send the frame, unchanged, back out of the AP to the other clients.
    pub send_to_other_clients: bool,
    /// Call [`Bridge::rewrite_client_frame_for_uplink`], then send it to the uplink.
    pub send_to_uplink: bool,
    /// Hand a copy to the ESP.
    pub send_to_esp: bool,
}

/// What to do with a frame received from the uplink (already rewritten in place).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UplinkFrameAction {
    Drop,
    SendToClients,
    /// The frame now holds a proxy-ARP reply: send it back to the uplink.
    ReplyToUplink,
    SendToEsp,
    SendToClientsAndEsp,
}

/// Where a frame sent by the ESP goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EspFrameDestinations {
    pub send_to_clients: bool,
    pub send_to_uplink: bool,
}

/// A client address the bridge currently knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KnownClientAddress {
    pub ip: Ipv4Bytes,
    pub mac: MacAddress,
    pub expires_in_s: u32,
}

#[derive(Clone, Copy, Debug, Default)]
struct ClientAddress {
    ip: Ipv4Bytes,
    mac: MacAddress,
    expires_at_s: u32,
}

impl ClientAddress {
    fn is_empty(&self) -> bool {
        self.ip == [0; 4]
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ClientHostname {
    mac: MacAddress,
    length: u8,
    bytes: [u8; MAX_HOSTNAME_LEN],
}

pub struct Bridge {
    station_mac: MacAddress,
    access_point_mac: MacAddress,
    esp_ip: Option<Ipv4Bytes>,
    client_addresses: [ClientAddress; MAX_CLIENT_ADDRESSES],
    client_hostnames: [ClientHostname; MAX_CLIENT_HOSTNAMES],
    next_hostname_slot: usize,
}

impl Bridge {
    pub fn new(station_mac: MacAddress, access_point_mac: MacAddress) -> Self {
        Self {
            station_mac,
            access_point_mac,
            esp_ip: None,
            client_addresses: [ClientAddress::default(); MAX_CLIENT_ADDRESSES],
            client_hostnames: [ClientHostname::default(); MAX_CLIENT_HOSTNAMES],
            next_hostname_slot: 0,
        }
    }

    /// The ESP's own IPv4 address, once it has one.
    pub fn set_esp_ip(&mut self, ip: Option<Ipv4Bytes>) {
        self.esp_ip = ip;
        if let Some(ip) = ip {
            self.forget_ip(ip);
        }
    }

    /// Handle a frame received from a client. `uptime_s` is a monotonic clock in seconds.
    pub fn handle_frame_from_client(&mut self, frame: &[u8], uptime_s: u32) -> ClientFrameAction {
        let Some(ethertype) = ethertype(frame) else { return ClientFrameAction::default() };
        let source_mac: MacAddress = frame[6..12].try_into().unwrap();
        let mut is_arp_for_esp = false;
        match ethertype {
            ETHERTYPE_ARP if frame.len() >= ETHERNET_HEADER_LEN + ARP_PACKET_LEN => {
                let sender_ip = read_ip(frame, ETHERNET_HEADER_LEN + 14);
                let target_ip = read_ip(frame, ETHERNET_HEADER_LEN + 24);
                self.remember_client_address(sender_ip, source_mac, uptime_s);
                is_arp_for_esp = self.is_esp_ip(target_ip);
            }
            ETHERTYPE_IPV4 if frame.len() >= ETHERNET_HEADER_LEN + IPV4_MIN_HEADER_LEN => {
                let source_ip = read_ip(frame, ETHERNET_HEADER_LEN + 12);
                self.remember_client_address(source_ip, source_mac, uptime_s);
                if let Some((_, dhcp_start)) = find_dhcp_payload(frame, DHCP_CLIENT_PORT, DHCP_SERVER_PORT)
                    && frame[dhcp_start] == BOOTP_REQUEST
                {
                    self.remember_client_hostname(source_mac, &frame[dhcp_start + DHCP_FIXED_PART_LEN..]);
                }
            }
            _ => return ClientFrameAction::default(),
        }

        let destination_mac: MacAddress = frame[0..6].try_into().unwrap();
        let only_to_esp = ClientFrameAction { send_to_esp: true, ..ClientFrameAction::default() };
        if is_arp_for_esp {
            // Nobody else needs to see an ARP for the ESP itself.
            only_to_esp
        } else if is_multicast(&destination_mac) {
            ClientFrameAction { send_to_other_clients: true, send_to_uplink: true, send_to_esp: true }
        } else if destination_mac == self.station_mac || destination_mac == self.access_point_mac {
            only_to_esp
        } else if self.is_known_client_mac(&destination_mac) {
            ClientFrameAction { send_to_other_clients: true, ..ClientFrameAction::default() }
        } else {
            ClientFrameAction { send_to_uplink: true, ..ClientFrameAction::default() }
        }
    }

    /// Where to send a frame from the ESP.
    pub fn route_frame_from_esp(&self, frame: &[u8]) -> EspFrameDestinations {
        let Some(destination_mac) = frame.get(0..6) else { return EspFrameDestinations::default() };
        let destination_mac: MacAddress = destination_mac.try_into().unwrap();
        if is_multicast(&destination_mac) {
            EspFrameDestinations { send_to_clients: true, send_to_uplink: true }
        } else if self.is_known_client_mac(&destination_mac) {
            EspFrameDestinations { send_to_clients: true, send_to_uplink: false }
        } else {
            EspFrameDestinations { send_to_clients: false, send_to_uplink: true }
        }
    }

    /// Make a client frame look like it comes from the station.
    pub fn rewrite_client_frame_for_uplink(&self, frame: &mut [u8]) {
        let Some(ethertype) = ethertype(frame) else { return };
        frame[6..12].copy_from_slice(&self.station_mac);
        match ethertype {
            ETHERTYPE_ARP if frame.len() >= ETHERNET_HEADER_LEN + ARP_PACKET_LEN => {
                let arp_sender_mac = ETHERNET_HEADER_LEN + 8..ETHERNET_HEADER_LEN + 14;
                frame[arp_sender_mac].copy_from_slice(&self.station_mac);
            }
            ETHERTYPE_IPV4 => {
                if let Some((udp_start, dhcp_start)) = find_dhcp_payload(frame, DHCP_CLIENT_PORT, DHCP_SERVER_PORT)
                    && frame[dhcp_start] == BOOTP_REQUEST
                {
                    // Broadcast flag: the server can't unicast the offer to a
                    // MAC that isn't on its side of the bridge.
                    frame[dhcp_start + 10] |= 0x80;
                    // A zero UDP checksum means "none" in IPv4.
                    frame[udp_start + 6..udp_start + 8].fill(0);
                }
            }
            _ => {}
        }
    }

    /// Handle a frame received from the uplink, rewriting it in place.
    pub fn handle_frame_from_uplink(&mut self, frame: &mut [u8], uptime_s: u32) -> UplinkFrameAction {
        let Some(ethertype) = ethertype(frame) else { return UplinkFrameAction::Drop };
        let destination_mac: MacAddress = frame[0..6].try_into().unwrap();
        let source_mac: MacAddress = frame[6..12].try_into().unwrap();
        if source_mac == self.station_mac {
            // Our own broadcast, echoed back by the access point.
            return UplinkFrameAction::Drop;
        }
        let is_broadcast = is_multicast(&destination_mac);
        if !is_broadcast && destination_mac != self.station_mac {
            return UplinkFrameAction::Drop;
        }

        match ethertype {
            ETHERTYPE_ARP if frame.len() >= ETHERNET_HEADER_LEN + ARP_PACKET_LEN => {
                let arp = ETHERNET_HEADER_LEN;
                let operation = u16::from_be_bytes([frame[arp + 6], frame[arp + 7]]);
                let sender_ip = read_ip(frame, arp + 14);
                let target_ip = read_ip(frame, arp + 24);
                // The sender IP lives on the uplink now (e.g. the client roamed back).
                if sender_ip != [0; 4] {
                    self.forget_ip(sender_ip);
                }
                if self.is_esp_ip(target_ip) {
                    return UplinkFrameAction::SendToEsp;
                }
                if operation == ARP_REQUEST && self.mac_for_ip(target_ip, uptime_s).is_some() {
                    self.turn_into_proxy_arp_reply(frame);
                    return UplinkFrameAction::ReplyToUplink;
                }
                if is_broadcast {
                    return UplinkFrameAction::SendToClients;
                }
                let Some(client_mac) = self.mac_for_ip(target_ip, uptime_s) else { return UplinkFrameAction::Drop };
                frame[0..6].copy_from_slice(&client_mac);
                frame[arp + 18..arp + 24].copy_from_slice(&client_mac);
                UplinkFrameAction::SendToClients
            }
            ETHERTYPE_IPV4 if frame.len() >= ETHERNET_HEADER_LEN + IPV4_MIN_HEADER_LEN => {
                let dhcp_client_mac = self.inspect_dhcp_reply(frame, uptime_s);
                if dhcp_client_mac == Some(self.station_mac) {
                    // A reply to the ESP's own DHCP client.
                    return UplinkFrameAction::SendToEsp;
                }
                if is_broadcast {
                    return UplinkFrameAction::SendToClientsAndEsp;
                }
                let destination_ip = read_ip(frame, ETHERNET_HEADER_LEN + 16);
                if self.is_esp_ip(destination_ip) {
                    return UplinkFrameAction::SendToEsp;
                }
                let client_mac = dhcp_client_mac.or_else(|| self.mac_for_ip(destination_ip, uptime_s));
                let Some(client_mac) = client_mac else { return UplinkFrameAction::Drop };
                frame[0..6].copy_from_slice(&client_mac);
                UplinkFrameAction::SendToClients
            }
            _ => UplinkFrameAction::Drop,
        }
    }

    /// A client left the AP: stop claiming its addresses on the uplink.
    pub fn forget_client(&mut self, mac: &MacAddress) {
        for address in self.client_addresses.iter_mut().filter(|address| address.mac == *mac) {
            *address = ClientAddress::default();
        }
    }

    /// The hostname a client sent in its last DHCP request.
    pub fn client_hostname(&self, mac: &MacAddress) -> Option<&str> {
        let hostname = self.client_hostnames.iter().find(|hostname| hostname.length > 0 && hostname.mac == *mac)?;
        core::str::from_utf8(&hostname.bytes[..usize::from(hostname.length)]).ok()
    }

    /// The client addresses that haven't expired.
    pub fn known_client_addresses(&self, uptime_s: u32) -> impl Iterator<Item = KnownClientAddress> + '_ {
        self.client_addresses
            .iter()
            .filter(move |address| !address.is_empty() && address.expires_at_s > uptime_s)
            .map(move |address| KnownClientAddress {
                ip: address.ip,
                mac: address.mac,
                expires_in_s: address.expires_at_s - uptime_s,
            })
    }

    fn remember_client_address(&mut self, ip: Ipv4Bytes, mac: MacAddress, uptime_s: u32) {
        let is_unicast_ip = ip != [0; 4] && ip != [255; 4] && ip[0] < 224;
        if !is_unicast_ip || is_multicast(&mac) || self.is_esp_ip(ip) {
            return;
        }
        let expires_at_s = uptime_s.saturating_add(CLIENT_ADDRESS_LIFETIME_S);
        if let Some(address) = self.client_addresses.iter_mut().find(|address| address.ip == ip) {
            address.mac = mac;
            address.expires_at_s = expires_at_s;
            return;
        }
        // Reuse a free or expired slot, else evict the one closest to expiry.
        let slot = self
            .client_addresses
            .iter_mut()
            .min_by_key(|address| {
                if address.is_empty() || address.expires_at_s <= uptime_s { 0 } else { address.expires_at_s }
            })
            .unwrap();
        *slot = ClientAddress { ip, mac, expires_at_s };
    }

    fn remember_client_hostname(&mut self, mac: MacAddress, dhcp_options: &[u8]) {
        let Some(hostname) = find_dhcp_option(dhcp_options, DHCP_OPTION_HOSTNAME) else { return };
        let hostname = &hostname[..hostname.len().min(MAX_HOSTNAME_LEN)];
        if hostname.is_empty() || core::str::from_utf8(hostname).is_err() {
            return;
        }
        let existing_slot = self.client_hostnames.iter().position(|entry| entry.length > 0 && entry.mac == mac);
        let slot = existing_slot.unwrap_or_else(|| {
            let slot = self.next_hostname_slot;
            self.next_hostname_slot = (slot + 1) % MAX_CLIENT_HOSTNAMES;
            slot
        });
        let entry = &mut self.client_hostnames[slot];
        entry.mac = mac;
        entry.length = hostname.len() as u8;
        entry.bytes[..hostname.len()].copy_from_slice(hostname);
    }

    fn is_esp_ip(&self, ip: Ipv4Bytes) -> bool {
        self.esp_ip == Some(ip)
    }

    fn mac_for_ip(&self, ip: Ipv4Bytes, uptime_s: u32) -> Option<MacAddress> {
        self.client_addresses
            .iter()
            .find(|address| address.ip == ip && address.expires_at_s > uptime_s)
            .map(|address| address.mac)
    }

    fn is_known_client_mac(&self, mac: &MacAddress) -> bool {
        self.client_addresses.iter().any(|address| !address.is_empty() && address.mac == *mac)
    }

    fn forget_ip(&mut self, ip: Ipv4Bytes) {
        for address in self.client_addresses.iter_mut().filter(|address| address.ip == ip) {
            *address = ClientAddress::default();
        }
    }

    /// Turn an ARP request into the reply "target IP is at the station MAC".
    fn turn_into_proxy_arp_reply(&self, frame: &mut [u8]) {
        let arp = ETHERNET_HEADER_LEN;
        let requester_mac: MacAddress = frame[arp + 8..arp + 14].try_into().unwrap();
        let requester_ip = read_ip(frame, arp + 14);
        let target_ip = read_ip(frame, arp + 24);
        frame[0..6].copy_from_slice(&requester_mac);
        frame[6..12].copy_from_slice(&self.station_mac);
        frame[arp + 6..arp + 8].copy_from_slice(&ARP_REPLY.to_be_bytes());
        frame[arp + 8..arp + 14].copy_from_slice(&self.station_mac);
        frame[arp + 14..arp + 18].copy_from_slice(&target_ip);
        frame[arp + 18..arp + 24].copy_from_slice(&requester_mac);
        frame[arp + 24..arp + 28].copy_from_slice(&requester_ip);
    }

    /// For a DHCP server reply, return the client's MAC, and remember the
    /// leased address on ACK.
    fn inspect_dhcp_reply(&mut self, frame: &[u8], uptime_s: u32) -> Option<MacAddress> {
        let (_, dhcp_start) = find_dhcp_payload(frame, DHCP_SERVER_PORT, DHCP_CLIENT_PORT)?;
        let hardware_address_len = frame[dhcp_start + 2];
        if frame[dhcp_start] != BOOTP_REPLY || hardware_address_len != 6 {
            return None;
        }
        let client_mac: MacAddress = frame[dhcp_start + 28..dhcp_start + 34].try_into().unwrap();
        let leased_ip = read_ip(frame, dhcp_start + 16);
        let message_type = dhcp_message_type(&frame[dhcp_start + DHCP_FIXED_PART_LEN..]);
        if client_mac != self.station_mac && message_type == Some(DHCP_ACK) {
            self.remember_client_address(leased_ip, client_mac, uptime_s);
        }
        Some(client_mac)
    }
}

fn ethertype(frame: &[u8]) -> Option<u16> {
    (frame.len() >= ETHERNET_HEADER_LEN).then(|| u16::from_be_bytes([frame[12], frame[13]]))
}

/// Multicast and broadcast MACs have the lowest bit of the first byte set.
fn is_multicast(mac: &MacAddress) -> bool {
    mac[0] & 1 != 0
}

fn read_ip(frame: &[u8], offset: usize) -> Ipv4Bytes {
    frame[offset..offset + 4].try_into().unwrap()
}

/// Offsets of the UDP header and of the DHCP payload, if `frame` is IPv4/UDP
/// between these ports.
fn find_dhcp_payload(frame: &[u8], source_port: u16, destination_port: u16) -> Option<(usize, usize)> {
    let ip_header_len = usize::from(frame.get(ETHERNET_HEADER_LEN)? & 0x0f) * 4;
    if ip_header_len < IPV4_MIN_HEADER_LEN || *frame.get(ETHERNET_HEADER_LEN + 9)? != IP_PROTOCOL_UDP {
        return None;
    }
    let udp_start = ETHERNET_HEADER_LEN + ip_header_len;
    let dhcp_start = udp_start + 8;
    if frame.len() < dhcp_start + DHCP_FIXED_PART_LEN
        || frame[udp_start..udp_start + 2] != source_port.to_be_bytes()
        || frame[udp_start + 2..udp_start + 4] != destination_port.to_be_bytes()
        || frame[dhcp_start + 236..dhcp_start + 240] != DHCP_MAGIC_COOKIE
    {
        return None;
    }
    Some((udp_start, dhcp_start))
}

fn dhcp_message_type(dhcp_options: &[u8]) -> Option<u8> {
    find_dhcp_option(dhcp_options, DHCP_OPTION_MESSAGE_TYPE)?.first().copied()
}

/// Value of a DHCP option, from the options that follow the magic cookie.
pub fn find_dhcp_option(mut dhcp_options: &[u8], option_code: u8) -> Option<&[u8]> {
    const PAD: u8 = 0;
    const END: u8 = 255;
    while let [code, rest @ ..] = dhcp_options {
        match *code {
            PAD => dhcp_options = rest,
            END => return None,
            _ => {
                let (&value_len, rest) = rest.split_first()?;
                let value = rest.get(..usize::from(value_len))?;
                if *code == option_code {
                    return Some(value);
                }
                dhcp_options = &rest[usize::from(value_len)..];
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATION: MacAddress = [0x02, 0, 0, 0, 0, 0x01];
    const ACCESS_POINT: MacAddress = [0x02, 0, 0, 0, 0, 0x02];
    const PHONE: MacAddress = [0xaa, 0, 0, 0, 0, 0x01];
    const LAPTOP: MacAddress = [0xaa, 0, 0, 0, 0, 0x02];
    const FREEBOX: MacAddress = [0xf0, 0, 0, 0, 0, 0x01];
    const BROADCAST: MacAddress = [0xff; 6];
    const PHONE_IP: Ipv4Bytes = [192, 168, 1, 50];
    const LAPTOP_IP: Ipv4Bytes = [192, 168, 1, 51];
    const FREEBOX_IP: Ipv4Bytes = [192, 168, 1, 254];
    const ESP_IP: Ipv4Bytes = [192, 168, 1, 60];
    const DHCP_REQUEST: u8 = 3;

    fn ethernet_frame(destination: MacAddress, source: MacAddress, ethertype: u16, payload: &[u8]) -> Vec<u8> {
        [&destination[..], &source[..], &ethertype.to_be_bytes(), payload].concat()
    }

    fn arp_packet(
        operation: u16,
        sender_mac: MacAddress,
        sender_ip: Ipv4Bytes,
        target_mac: MacAddress,
        target_ip: Ipv4Bytes,
    ) -> Vec<u8> {
        let ethernet_ipv4_header = [0, 1, 8, 0, 6, 4];
        [&ethernet_ipv4_header[..], &operation.to_be_bytes(), &sender_mac, &sender_ip, &target_mac, &target_ip].concat()
    }

    fn udp_packet(source_ip: Ipv4Bytes, destination_ip: Ipv4Bytes, source_port: u16, destination_port: u16, payload: &[u8]) -> Vec<u8> {
        let total_len = (28 + payload.len()) as u16;
        let mut ip_header = vec![0x45, 0, 0, 0, 0, 0, 0, 0, 64, IP_PROTOCOL_UDP, 0, 0];
        ip_header[2..4].copy_from_slice(&total_len.to_be_bytes());
        let udp_len = (8 + payload.len()) as u16;
        let checksum = [0x12, 0x34];
        let udp_header =
            [&source_port.to_be_bytes()[..], &destination_port.to_be_bytes(), &udp_len.to_be_bytes(), &checksum].concat();
        [&ip_header[..], &source_ip, &destination_ip, &udp_header, payload].concat()
    }

    fn dhcp_message(bootp_operation: u8, client_mac: MacAddress, leased_ip: Ipv4Bytes, message_type: u8) -> Vec<u8> {
        dhcp_message_with_hostname(bootp_operation, client_mac, leased_ip, message_type, b"")
    }

    fn dhcp_message_with_hostname(
        bootp_operation: u8,
        client_mac: MacAddress,
        leased_ip: Ipv4Bytes,
        message_type: u8,
        hostname: &[u8],
    ) -> Vec<u8> {
        let mut message = vec![0u8; DHCP_FIXED_PART_LEN];
        message[0] = bootp_operation;
        message[1] = 1; // Ethernet
        message[2] = 6; // MAC length
        message[16..20].copy_from_slice(&leased_ip);
        message[28..34].copy_from_slice(&client_mac);
        message[236..240].copy_from_slice(&DHCP_MAGIC_COOKIE);
        message.extend_from_slice(&[DHCP_OPTION_MESSAGE_TYPE, 1, message_type]);
        if !hostname.is_empty() {
            message.extend_from_slice(&[DHCP_OPTION_HOSTNAME, hostname.len() as u8]);
            message.extend_from_slice(hostname);
        }
        message.push(255);
        message
    }

    fn new_bridge() -> Bridge {
        Bridge::new(STATION, ACCESS_POINT)
    }

    #[test]
    fn client_arp_goes_up_as_station_and_is_remembered() {
        let mut bridge = new_bridge();
        let mut frame = ethernet_frame(BROADCAST, PHONE, ETHERTYPE_ARP, &arp_packet(ARP_REQUEST, PHONE, PHONE_IP, [0; 6], FREEBOX_IP));
        let action = bridge.handle_frame_from_client(&frame, 0);
        assert_eq!(action, ClientFrameAction { send_to_other_clients: true, send_to_uplink: true, send_to_esp: true });
        bridge.rewrite_client_frame_for_uplink(&mut frame);
        assert_eq!(frame[6..12], STATION);
        assert_eq!(frame[22..28], STATION);
        assert_eq!(bridge.mac_for_ip(PHONE_IP, 0), Some(PHONE));
    }

    #[test]
    fn uplink_arp_reply_reaches_client() {
        let mut bridge = new_bridge();
        bridge.remember_client_address(PHONE_IP, PHONE, 0);
        let mut frame = ethernet_frame(STATION, FREEBOX, ETHERTYPE_ARP, &arp_packet(ARP_REPLY, FREEBOX, FREEBOX_IP, STATION, PHONE_IP));
        assert_eq!(bridge.handle_frame_from_uplink(&mut frame, 1), UplinkFrameAction::SendToClients);
        assert_eq!(frame[0..6], PHONE);
        assert_eq!(frame[6..12], FREEBOX);
        assert_eq!(frame[32..38], PHONE);
    }

    #[test]
    fn proxy_arp_for_client_ip() {
        let mut bridge = new_bridge();
        bridge.remember_client_address(PHONE_IP, PHONE, 0);
        let mut frame = ethernet_frame(BROADCAST, FREEBOX, ETHERTYPE_ARP, &arp_packet(ARP_REQUEST, FREEBOX, FREEBOX_IP, [0; 6], PHONE_IP));
        assert_eq!(bridge.handle_frame_from_uplink(&mut frame, 1), UplinkFrameAction::ReplyToUplink);
        let expected_reply = arp_packet(ARP_REPLY, STATION, PHONE_IP, FREEBOX, FREEBOX_IP);
        assert_eq!(frame, ethernet_frame(FREEBOX, STATION, ETHERTYPE_ARP, &expected_reply));
    }

    #[test]
    fn uplink_claim_of_client_ip_forgets_it() {
        let mut bridge = new_bridge();
        bridge.remember_client_address(PHONE_IP, PHONE, 0);
        // The phone roamed back to the Freebox and announces itself there.
        let mut frame = ethernet_frame(BROADCAST, PHONE, ETHERTYPE_ARP, &arp_packet(ARP_REQUEST, PHONE, PHONE_IP, [0; 6], PHONE_IP));
        assert_eq!(bridge.handle_frame_from_uplink(&mut frame, 1), UplinkFrameAction::SendToClients);
        assert_eq!(bridge.mac_for_ip(PHONE_IP, 1), None);
    }

    #[test]
    fn unicast_ip_is_steered_by_destination_ip() {
        let mut bridge = new_bridge();
        bridge.remember_client_address(PHONE_IP, PHONE, 0);
        bridge.remember_client_address(LAPTOP_IP, LAPTOP, 0);
        let mut frame = ethernet_frame(STATION, FREEBOX, ETHERTYPE_IPV4, &udp_packet(FREEBOX_IP, LAPTOP_IP, 53, 4000, b"x"));
        assert_eq!(bridge.handle_frame_from_uplink(&mut frame, 1), UplinkFrameAction::SendToClients);
        assert_eq!(frame[0..6], LAPTOP);
        let unknown_ip = [192, 168, 1, 99];
        let mut frame = ethernet_frame(STATION, FREEBOX, ETHERTYPE_IPV4, &udp_packet(FREEBOX_IP, unknown_ip, 53, 4000, b"x"));
        assert_eq!(bridge.handle_frame_from_uplink(&mut frame, 1), UplinkFrameAction::Drop);
    }

    #[test]
    fn echoed_broadcast_and_ipv6_are_dropped() {
        let mut bridge = new_bridge();
        let mut frame = ethernet_frame(BROADCAST, STATION, ETHERTYPE_ARP, &arp_packet(ARP_REQUEST, STATION, PHONE_IP, [0; 6], FREEBOX_IP));
        assert_eq!(bridge.handle_frame_from_uplink(&mut frame, 0), UplinkFrameAction::Drop);
        const ETHERTYPE_IPV6: u16 = 0x86dd;
        let mut frame = ethernet_frame([0x33, 0x33, 0, 0, 0, 1], FREEBOX, ETHERTYPE_IPV6, &[0; 40]);
        assert_eq!(bridge.handle_frame_from_uplink(&mut frame, 0), UplinkFrameAction::Drop);
        let frame = ethernet_frame(BROADCAST, PHONE, ETHERTYPE_IPV6, &[0; 40]);
        assert_eq!(bridge.handle_frame_from_client(&frame, 0), ClientFrameAction::default());
    }

    #[test]
    fn dhcp_request_keeps_client_mac_and_asks_for_broadcast() {
        let mut bridge = new_bridge();
        let request = dhcp_message(BOOTP_REQUEST, PHONE, [0; 4], DHCP_REQUEST);
        let mut frame = ethernet_frame(BROADCAST, PHONE, ETHERTYPE_IPV4, &udp_packet([0; 4], [255; 4], 68, 67, &request));
        let action = bridge.handle_frame_from_client(&frame, 0);
        assert_eq!(action, ClientFrameAction { send_to_other_clients: true, send_to_uplink: true, send_to_esp: true });
        bridge.rewrite_client_frame_for_uplink(&mut frame);
        let dhcp_start = ETHERNET_HEADER_LEN + 28;
        assert_eq!(frame[6..12], STATION);
        assert_eq!(frame[dhcp_start + 28..dhcp_start + 34], PHONE);
        assert_eq!(frame[dhcp_start + 10] & 0x80, 0x80);
        let udp_checksum = ETHERNET_HEADER_LEN + 26..ETHERNET_HEADER_LEN + 28;
        assert_eq!(frame[udp_checksum], [0, 0]);
    }

    #[test]
    fn dhcp_ack_remembers_lease_and_unicast_reply_reaches_client() {
        let mut bridge = new_bridge();
        let ack = dhcp_message(BOOTP_REPLY, PHONE, PHONE_IP, DHCP_ACK);
        let mut frame = ethernet_frame(STATION, FREEBOX, ETHERTYPE_IPV4, &udp_packet(FREEBOX_IP, PHONE_IP, 67, 68, &ack));
        assert_eq!(bridge.handle_frame_from_uplink(&mut frame, 0), UplinkFrameAction::SendToClients);
        assert_eq!(frame[0..6], PHONE);
        assert_eq!(bridge.mac_for_ip(PHONE_IP, 0), Some(PHONE));
    }

    #[test]
    fn client_to_client_unicast_stays_on_access_point() {
        let mut bridge = new_bridge();
        bridge.remember_client_address(LAPTOP_IP, LAPTOP, 0);
        let frame = ethernet_frame(LAPTOP, PHONE, ETHERTYPE_IPV4, &udp_packet(PHONE_IP, LAPTOP_IP, 1, 2, b"x"));
        let action = bridge.handle_frame_from_client(&frame, 0);
        assert_eq!(action, ClientFrameAction { send_to_other_clients: true, ..ClientFrameAction::default() });
    }

    #[test]
    fn addresses_expire_and_leave_with_the_client() {
        let mut bridge = new_bridge();
        bridge.remember_client_address(PHONE_IP, PHONE, 0);
        bridge.remember_client_address(LAPTOP_IP, LAPTOP, 0);
        assert_eq!(bridge.mac_for_ip(PHONE_IP, CLIENT_ADDRESS_LIFETIME_S), None);
        bridge.forget_client(&LAPTOP);
        assert_eq!(bridge.mac_for_ip(LAPTOP_IP, 1), None);
        assert_eq!(bridge.known_client_addresses(1).count(), 1);
    }

    #[test]
    fn esp_dhcp_reply_goes_to_esp_only() {
        let mut bridge = new_bridge();
        let ack = dhcp_message(BOOTP_REPLY, STATION, ESP_IP, DHCP_ACK);
        let mut frame = ethernet_frame(BROADCAST, FREEBOX, ETHERTYPE_IPV4, &udp_packet(FREEBOX_IP, [255; 4], 67, 68, &ack));
        assert_eq!(bridge.handle_frame_from_uplink(&mut frame, 0), UplinkFrameAction::SendToEsp);
        assert_eq!(bridge.known_client_addresses(0).count(), 0);
    }

    #[test]
    fn uplink_traffic_for_esp_ip_goes_to_esp() {
        let mut bridge = new_bridge();
        bridge.set_esp_ip(Some(ESP_IP));
        let mut frame = ethernet_frame(BROADCAST, FREEBOX, ETHERTYPE_ARP, &arp_packet(ARP_REQUEST, FREEBOX, FREEBOX_IP, [0; 6], ESP_IP));
        assert_eq!(bridge.handle_frame_from_uplink(&mut frame, 0), UplinkFrameAction::SendToEsp);
        let mut frame = ethernet_frame(STATION, FREEBOX, ETHERTYPE_IPV4, &udp_packet(FREEBOX_IP, ESP_IP, 1, 80, b"x"));
        assert_eq!(bridge.handle_frame_from_uplink(&mut frame, 0), UplinkFrameAction::SendToEsp);
        let mdns_group_mac = [1, 0, 0x5e, 0, 0, 0xfb];
        let mdns_query = udp_packet(FREEBOX_IP, [224, 0, 0, 251], 5353, 5353, b"q");
        let mut frame = ethernet_frame(mdns_group_mac, FREEBOX, ETHERTYPE_IPV4, &mdns_query);
        assert_eq!(bridge.handle_frame_from_uplink(&mut frame, 0), UplinkFrameAction::SendToClientsAndEsp);
    }

    #[test]
    fn client_traffic_for_esp_goes_to_esp() {
        let mut bridge = new_bridge();
        bridge.set_esp_ip(Some(ESP_IP));
        let only_to_esp = ClientFrameAction { send_to_esp: true, ..ClientFrameAction::default() };
        let frame = ethernet_frame(BROADCAST, PHONE, ETHERTYPE_ARP, &arp_packet(ARP_REQUEST, PHONE, PHONE_IP, [0; 6], ESP_IP));
        assert_eq!(bridge.handle_frame_from_client(&frame, 0), only_to_esp);
        let frame = ethernet_frame(STATION, PHONE, ETHERTYPE_IPV4, &udp_packet(PHONE_IP, ESP_IP, 4000, 80, b"x"));
        assert_eq!(bridge.handle_frame_from_client(&frame, 0), only_to_esp);
        // The ESP's own address is never remembered as a client's.
        assert_eq!(bridge.mac_for_ip(ESP_IP, 0), None);
    }

    #[test]
    fn esp_frames_are_routed_by_destination_mac() {
        let mut bridge = new_bridge();
        bridge.remember_client_address(PHONE_IP, PHONE, 0);
        let to_phone = ethernet_frame(PHONE, STATION, ETHERTYPE_IPV4, &[0; 20]);
        let to_freebox = ethernet_frame(FREEBOX, STATION, ETHERTYPE_IPV4, &[0; 20]);
        let broadcast = ethernet_frame(BROADCAST, STATION, ETHERTYPE_ARP, &[0; 28]);
        let only_clients = EspFrameDestinations { send_to_clients: true, send_to_uplink: false };
        let only_uplink = EspFrameDestinations { send_to_clients: false, send_to_uplink: true };
        let both = EspFrameDestinations { send_to_clients: true, send_to_uplink: true };
        assert_eq!(bridge.route_frame_from_esp(&to_phone), only_clients);
        assert_eq!(bridge.route_frame_from_esp(&to_freebox), only_uplink);
        assert_eq!(bridge.route_frame_from_esp(&broadcast), both);
    }

    #[test]
    fn client_hostname_is_remembered_from_dhcp_request() {
        let mut bridge = new_bridge();
        let request = dhcp_message_with_hostname(BOOTP_REQUEST, PHONE, [0; 4], DHCP_REQUEST, b"pixel-7");
        let frame = ethernet_frame(BROADCAST, PHONE, ETHERTYPE_IPV4, &udp_packet([0; 4], [255; 4], 68, 67, &request));
        bridge.handle_frame_from_client(&frame, 0);
        assert_eq!(bridge.client_hostname(&PHONE), Some("pixel-7"));
        assert_eq!(bridge.client_hostname(&LAPTOP), None);
    }

    #[test]
    fn full_address_table_evicts_the_oldest() {
        let mut bridge = new_bridge();
        for i in 0..MAX_CLIENT_ADDRESSES as u8 {
            bridge.remember_client_address([10, 0, 0, i + 1], [0xaa, 0, 0, 0, 0, i], u32::from(i));
        }
        bridge.remember_client_address([10, 0, 1, 1], PHONE, 100);
        assert_eq!(bridge.mac_for_ip([10, 0, 0, 1], 100), None);
        assert_eq!(bridge.mac_for_ip([10, 0, 1, 1], 100), Some(PHONE));
    }
}
