//! A virtual Ethernet device for the ESP's own IP stack ("the ESP").
//!
//! The forwarding loop copies frames meant for the ESP into one queue and
//! routes whatever the ESP leaves in the other, so the bridge's fast path never
//! waits on the IP stack. Frames are written once, in place.

use bridge_core::MacAddress;
use core::task::{Context, Poll};
use embassy_net_driver::{Capabilities, Driver, HardwareAddress, LinkState, RxToken, TxToken};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::zerocopy_channel::{Channel, Receiver, Sender};
use static_cell::{ConstStaticCell, StaticCell};

pub const MAX_FRAME_LEN: usize = 1514;
const QUEUE_LEN: usize = 4;

pub struct Frame {
    len: usize,
    bytes: [u8; MAX_FRAME_LEN],
}

impl Frame {
    const EMPTY: Self = Self { len: 0, bytes: [0; MAX_FRAME_LEN] };

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

type FrameSender = Sender<'static, CriticalSectionRawMutex, Frame>;
type FrameReceiver = Receiver<'static, CriticalSectionRawMutex, Frame>;

/// The forwarding loop's ends of the two queues.
pub struct EspQueues {
    to_esp: FrameSender,
    from_esp: FrameReceiver,
}

impl EspQueues {
    /// Hand a frame to the ESP; returns false if its queue is full.
    pub fn send_to_esp(&mut self, frame: &[u8]) -> bool {
        let Some(slot) = self.to_esp.try_send() else { return false };
        let Some(destination) = slot.bytes.get_mut(..frame.len()) else { return false };
        destination.copy_from_slice(frame);
        slot.len = frame.len();
        self.to_esp.send_done();
        true
    }

    /// Run `handle_frame` on the next frame from the ESP, if any. Returns
    /// whether there was one.
    pub fn handle_frame_from_esp(
        &mut self,
        context: &mut Context<'_>,
        handle_frame: impl FnOnce(&[u8], &mut Context<'_>),
    ) -> bool {
        match self.from_esp.poll_receive(context) {
            Poll::Ready(frame) => {
                handle_frame(frame.as_slice(), context);
                self.from_esp.receive_done();
                true
            }
            Poll::Pending => false,
        }
    }
}

/// The device `embassy_net` drives: it reads the `to_esp` queue and writes
/// the `from_esp` queue.
pub struct EspNetworkDevice {
    mac: MacAddress,
    incoming: FrameReceiver,
    outgoing: FrameSender,
}

/// Create the device for `embassy_net` and the forwarding loop's queues. Call once.
pub fn new(mac: MacAddress) -> (EspNetworkDevice, EspQueues) {
    static TO_ESP_FRAMES: ConstStaticCell<[Frame; QUEUE_LEN]> = ConstStaticCell::new([Frame::EMPTY; QUEUE_LEN]);
    static FROM_ESP_FRAMES: ConstStaticCell<[Frame; QUEUE_LEN]> = ConstStaticCell::new([Frame::EMPTY; QUEUE_LEN]);
    static TO_ESP: StaticCell<Channel<'static, CriticalSectionRawMutex, Frame>> = StaticCell::new();
    static FROM_ESP: StaticCell<Channel<'static, CriticalSectionRawMutex, Frame>> = StaticCell::new();
    let (to_esp, incoming) = TO_ESP.init(Channel::new(TO_ESP_FRAMES.take())).split();
    let (outgoing, from_esp) = FROM_ESP.init(Channel::new(FROM_ESP_FRAMES.take())).split();
    (EspNetworkDevice { mac, incoming, outgoing }, EspQueues { to_esp, from_esp })
}

pub struct ReceivedFrame<'a>(&'a mut FrameReceiver);
pub struct FrameToSend<'a>(&'a mut FrameSender);

impl RxToken for ReceivedFrame<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, read_frame: F) -> R {
        let frame = self.0.try_receive().expect("receive() saw a frame");
        let result = read_frame(&mut frame.bytes[..frame.len]);
        self.0.receive_done();
        result
    }
}

impl TxToken for FrameToSend<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, write_frame: F) -> R {
        let frame = self.0.try_send().expect("transmit() saw room");
        frame.len = len.min(MAX_FRAME_LEN);
        let result = write_frame(&mut frame.bytes[..frame.len]);
        self.0.send_done();
        result
    }
}

impl Driver for EspNetworkDevice {
    type RxToken<'a> = ReceivedFrame<'a>;
    type TxToken<'a> = FrameToSend<'a>;

    fn receive(&mut self, context: &mut Context<'_>) -> Option<(ReceivedFrame<'_>, FrameToSend<'_>)> {
        if self.outgoing.poll_send(context).is_pending() || self.incoming.poll_receive(context).is_pending() {
            return None;
        }
        Some((ReceivedFrame(&mut self.incoming), FrameToSend(&mut self.outgoing)))
    }

    fn transmit(&mut self, context: &mut Context<'_>) -> Option<FrameToSend<'_>> {
        self.outgoing.poll_send(context).is_ready().then_some(FrameToSend(&mut self.outgoing))
    }

    fn link_state(&mut self, _context: &mut Context<'_>) -> LinkState {
        LinkState::Up
    }

    fn capabilities(&self) -> Capabilities {
        let mut capabilities = Capabilities::default();
        capabilities.max_transmission_unit = MAX_FRAME_LEN;
        capabilities.max_burst_size = Some(1);
        capabilities
    }

    fn hardware_address(&self) -> HardwareAddress {
        HardwareAddress::Ethernet(self.mac)
    }
}
