//! Queue of outgoing packets.

use MAX_WINDOW_SIZE;
use packet::{self, Packet, HEADER_LEN};

use std::{cmp, io};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

// TODO:
//
// * Nagle check, don't flush the last data packet if there is in-flight data
//   and it is too small.

#[derive(Debug)]
pub struct OutQueue {
    // queued packets
    packets: VecDeque<Entry>,

    state: State,

    // Round trip time in microseconds
    rtt: u64,
    rtt_variance: i64,

    // Max number of bytes that we can have in-flight to the peer w/o acking.
    // This number dynamically changes to handle control flow.
    max_window: u32,
}

#[derive(Debug)]
struct State {
    connection_id: u16,

    // Sequence number for the next packet
    seq_nr: u16,

    // Sequence number of the last locally acked packet (aka, read)
    local_ack: Option<u16>,

    // Last outbound ack
    last_ack: Option<u16>,

    // Peer's window. This is the number of bytes that it has locally but not
    // acked
    peer_window: u32,

    // This is the number of bytes available in our inbound receive queue.
    local_window: u32,

    // The instant at which the `OutQueue` was created, this is used as the
    // reference point when calculating the current timestamp in microseconds.
    created_at: Instant,

    // Difference between the `timestamp` specified by the last incoming packet
    // and the current time.
    their_delay: u32,
}

#[derive(Debug)]
struct Entry {
    packet: Packet,
    num_sends: u32,
    last_sent_at: Option<Instant>,
    acked: bool,
}

pub struct Next<'a> {
    item: Item<'a>,
    state: &'a mut State,
}

enum Item<'a> {
    Entry(&'a mut Entry),
    State(Packet),
}

// Max size of a UDP packet... ideally this will be dynamically discovered using
// MTU.
const MAX_PACKET_SIZE: usize = 1_400;
const MIN_PACKET_SIZE: usize = 150;

const MAX_DATA_SIZE: usize = MAX_PACKET_SIZE - HEADER_LEN;
const MIN_DATA_SIZE: usize = MIN_PACKET_SIZE - HEADER_LEN;

const MICROS_PER_SEC: u32 = 1_000_000;
const NANOS_PER_MS: u32 = 1_000_000;
const NANOS_PER_MICRO: u32 = 1_000;

impl OutQueue {
    /// Create a new `OutQueue` with the specified `seq_nr` and `ack_nr`
    pub fn new(connection_id: u16,
               seq_nr: u16,
               local_ack: Option<u16>) -> OutQueue
    {
        OutQueue {
            packets: VecDeque::new(),
            state: State {
                connection_id: connection_id,
                seq_nr: seq_nr,
                local_ack: local_ack,
                last_ack: None,
                peer_window: MAX_WINDOW_SIZE as u32,
                local_window: 0,
                created_at: Instant::now(),
                their_delay: 0,
            },
            rtt: 0,
            rtt_variance: 0,
            // Start the max window at the packet size
            max_window: MAX_PACKET_SIZE as u32,
        }
    }

    /// Returns true if the out queue is fully flushed and all packets have been
    /// ACKed.
    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    /// Whenever a packet is received, the included timestamp is passed in here.
    pub fn set_their_delay(&mut self, their_timestamp: u32) {
        let our_timestamp = as_micros(self.state.created_at.elapsed());
        self.state.their_delay = our_timestamp.wrapping_sub(their_timestamp);
    }

    pub fn set_their_ack(&mut self, ack_nr: u16) {
        let now = Instant::now();

        loop {
            let pop = self.packets.front()
                .map(|entry| {
                    let seq_nr = entry.packet.seq_nr();

                    let lower = ack_nr.wrapping_sub(ack_nr);

                    if lower < ack_nr {
                        seq_nr > lower && seq_nr <= ack_nr
                    } else {
                        seq_nr > lower || seq_nr <= ack_nr
                    }
                })
                .unwrap_or(false);

            if !pop {
                return;
            }

            // The packet has been acked..
            let p = self.packets.pop_front().unwrap();

            if p.num_sends == 1 {
                // Use the packet to update rtt & rtt_variance
                let packet_rtt = as_ms(now.duration_since(p.last_sent_at.unwrap()));
                let delta = (self.rtt as i64 - packet_rtt as i64).abs();

                self.rtt_variance += (delta - self.rtt_variance) / 4;

                if self.rtt >= packet_rtt {
                    self.rtt -= (self.rtt - packet_rtt) / 8;
                } else {
                    self.rtt += (packet_rtt - self.rtt) / 8;
                }
            }
        }
    }

    pub fn set_local_window(&mut self, val: usize) {
        assert!(val <= ::std::u32::MAX as usize);
        self.state.local_window = val as u32;
    }

    /// Update peer ack
    pub fn set_local_ack(&mut self, val: u16) {
        // TODO: Since STATE packets can be lost, if any packet is received from
        // the remote, *some* sort of state packet needs to be sent out in the
        // near term future.
        self.state.local_ack = Some(val);
    }

    /// Returns the socket timeout based on an aggregate of packet round trip
    /// times.
    pub fn socket_timeout(&self) -> Duration {
        let timeout = self.rtt as i64 + self.rtt_variance;

        if timeout > 500 {
            Duration::from_millis(timeout as u64)
        } else {
            Duration::from_millis(500)
        }
    }

    /// Push an outbound packet into the queue
    pub fn push(&mut self, mut packet: Packet) {
        assert!(packet.ty() != packet::Type::State);

        // SYN packets are special and will have the connection ID already
        // correctly set
        if packet.ty() != packet::Type::Syn {
            packet.set_connection_id(self.state.connection_id);
        }

        // Set the sequence number
        packet.set_seq_nr(self.state.seq_nr);

        // Increment the seq_nr
        self.state.seq_nr = self.state.seq_nr.wrapping_add(1);

        self.packets.push_back(Entry {
            packet: packet,
            num_sends: 0,
            last_sent_at: None,
            acked: false,
        });
    }

    pub fn next(&mut self) -> Option<Next> {
        let ts = self.timestamp();
        let diff = self.state.their_delay;
        let ack = self.state.local_ack.unwrap_or(0);
        let wnd_size = self.state.local_window;

        for entry in &mut self.packets {
            // The packet has been sent
            if entry.last_sent_at.is_some() {
                continue;
            }

            // Update timestamp
            entry.packet.set_timestamp(ts);
            entry.packet.set_timestamp_diff(diff);
            entry.packet.set_ack_nr(ack);
            entry.packet.set_wnd_size(wnd_size);

            return Some(Next {
                item: Item::Entry(entry),
                state: &mut self.state,
            });
        }

        if self.state.local_ack != self.state.last_ack {
            let mut packet = Packet::state();

            packet.set_connection_id(self.state.connection_id);
            packet.set_seq_nr(self.state.seq_nr);
            packet.set_timestamp(ts);
            packet.set_timestamp_diff(diff);
            packet.set_ack_nr(ack);
            packet.set_wnd_size(wnd_size);

            return Some(Next {
                item: Item::State(packet),
                state: &mut self.state,
            });
        }

        None
    }

    /// Push data into the outbound queue
    pub fn write(&mut self, mut src: &[u8]) -> io::Result<usize> {
        if src.len() == 0 {
            return Ok(0);
        }

        let cur_window = self.in_flight();
        let max = cmp::min(self.max_window, self.state.peer_window) as usize;

        if cur_window >= max {
            return Err(io::ErrorKind::WouldBlock.into());
        }

        let mut rem = max - cur_window;
        let mut len = 0;

        while rem > HEADER_LEN {
            let packet_len = cmp::min(
                MAX_PACKET_SIZE,
                cmp::min(src.len(), rem - HEADER_LEN));

            if packet_len == 0 {
                break;
            }

            let packet = Packet::data(&src[..packet_len]);
            self.push(packet);

            len += packet_len;
            rem -= packet_len + HEADER_LEN;

            src = &src[packet_len..];
        }

        Ok(len)
    }

    pub fn is_writable(&self) -> bool {
        self.buffered() < MAX_WINDOW_SIZE as usize
    }

    pub fn in_flight(&self) -> usize {
        // TODO: Don't iterate each time
        self.packets.iter()
            .filter(|p| p.last_sent_at.is_some() && !p.acked)
            .count()
    }

    pub fn buffered(&self) -> usize {
        // TODO: Don't iterate each time
        self.packets.iter()
            .map(|p| p.packet.payload().len())
            .sum()
    }

    fn timestamp(&self) -> u32 {
        as_micros(self.state.created_at.elapsed())
    }
}

impl<'a> Next<'a> {
    pub fn packet(&self) -> &Packet {
        match self.item {
            Item::Entry(ref e) => &e.packet,
            Item::State(ref p) => p,
        }
    }

    pub fn sent(mut self) {
        if let Item::Entry(ref mut e) = self.item {
            // Increment the number of sends
            e.num_sends += 1;

            // Track the time
            e.last_sent_at = Some(Instant::now());
        }

        self.state.last_ack = self.state.local_ack;
    }
}

fn as_micros(duration: Duration) -> u32 {
    // Wrapping is OK
    let mut ret = duration.as_secs().wrapping_mul(MICROS_PER_SEC as u64) as u32;
    ret += duration.subsec_nanos() / NANOS_PER_MICRO;
    ret
}

fn as_ms(duration: Duration) -> u64 {
    // Lets just limit to 30 seconds
    if duration.as_secs() > 30 {
        30_000
    } else {
        let sub_secs = duration.subsec_nanos() / NANOS_PER_MS;
        duration.as_secs() * 1000 + sub_secs as u64
    }
}