//! Repeater state shared between the tasks, read by the status page.

use bridge_core::{Ipv4Bytes, MAX_CLIENT_ADDRESSES, MAX_HOSTNAME_LEN, MacAddress};
use core::cell::RefCell;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

/// Clients associated with the AP that the page lists.
pub const MAX_ASSOCIATED_CLIENTS: usize = 16;

#[derive(Default, Clone)]
pub struct Uplink {
    pub connected: bool,
    /// Uptime (s) when the station last connected or lost the uplink.
    pub changed_at_s: u32,
    /// The access point's MAC.
    pub bssid: MacAddress,
    pub channel: u8,
    pub signal_dbm: i8,
    pub failed_attempts: u32,
}

/// Cumulative counters; the page derives rates from them.
#[derive(Default, Clone, Copy)]
pub struct Traffic {
    pub frames_from_uplink: u32,
    pub bytes_from_uplink: u64,
    pub frames_from_clients: u32,
    pub bytes_from_clients: u64,
    pub dropped_frames: u32,
    pub frames_from_esp: u32,
}

/// A client associated with the AP.
#[derive(Clone, Copy)]
pub struct AssociatedClient {
    pub mac: MacAddress,
    /// Uptime (s) when it joined.
    pub joined_at_s: u32,
}

/// An IP address the bridge knows for a client.
#[derive(Clone)]
pub struct ClientAddress {
    pub ip: Ipv4Bytes,
    pub mac: MacAddress,
    pub expires_in_s: u32,
    pub hostname: heapless::String<MAX_HOSTNAME_LEN>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Addressing {
    /// Waiting for a lease from the uplink's DHCP server.
    WaitingForLease,
    /// Leased from the uplink.
    Leased,
    /// Uplink down: fixed address and DHCP server on the AP.
    Fallback,
}

#[allow(clippy::large_stack_frames, reason = "a few KB of snapshot; the main stack is 80 KB")]
#[derive(Clone)]
pub struct Status {
    pub uplink: Uplink,
    pub associated_clients: heapless::Vec<AssociatedClient, MAX_ASSOCIATED_CLIENTS>,
    pub client_addresses: heapless::Vec<ClientAddress, MAX_CLIENT_ADDRESSES>,
    pub traffic: Traffic,
    pub addressing: Addressing,
    pub chip_temperature_c: f32,
}

static STATUS: Mutex<CriticalSectionRawMutex, RefCell<Status>> = Mutex::new(RefCell::new(Status {
    uplink: Uplink {
        connected: false,
        changed_at_s: 0,
        bssid: [0; 6],
        channel: 0,
        signal_dbm: 0,
        failed_attempts: 0,
    },
    associated_clients: heapless::Vec::new(),
    client_addresses: heapless::Vec::new(),
    traffic: Traffic {
        frames_from_uplink: 0,
        bytes_from_uplink: 0,
        frames_from_clients: 0,
        bytes_from_clients: 0,
        dropped_frames: 0,
        frames_from_esp: 0,
    },
    addressing: Addressing::WaitingForLease,
    chip_temperature_c: 0.0,
}));

/// Read or update the shared status. Keep `f` short: it runs with interrupts off.
pub fn with<R>(f: impl FnOnce(&mut Status) -> R) -> R {
    STATUS.lock(|status| f(&mut status.borrow_mut()))
}
