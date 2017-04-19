//! Tests are implemented in the source tree in order to have access to private
//! fields.

mod mock;
mod harness;

use packet::{self, Packet};
use self::mock::Mock;
use self::harness::Harness;

#[test]
fn connect_echo_close() {
    const CONNECTION_ID: u16 = 25103;

    let _ = ::env_logger::init();

    let socket = Harness::new();
    let mock = Mock::new();
    let server = mock.local_addr();

    let addr = socket.local_addr();
    let th = mock.background(move |m| {
        // Receive the SYN packet
        let packet = m.recv_from(&addr);

        assert_eq!(packet.ty(), packet::Type::Syn);
        assert_eq!(packet.version(), 1);
        assert_eq!(packet.seq_nr(), 1);
        assert_eq!(packet.ack_nr(), 0);

        let mut p = Packet::state();
        p.set_connection_id(CONNECTION_ID);
        p.set_seq_nr(123);
        p.set_ack_nr(1);

        // Send the STATE packet
        m.send_to(p, &addr);

        // No further packets sent on the socket
        m.assert_quiescence(1_000);
    });

    let stream = socket.connect(server);

    // The socket becomes writable
    socket.wait_until(|| stream.is_writable());

    // The socket should not be readable
    assert!(!stream.is_readable());

    // Wait for the server half
    let mock = th.join().unwrap();

    // Write some data
    let n = stream.write(b"hello world").unwrap();
    assert_eq!(n, 11);

    // Receive the data
    let addr = socket.local_addr();
    let th = mock.background(move |m| {
        // Receive the data packet
        let packet = m.recv_from(&addr);

        assert_eq!(packet.ty(), packet::Type::Data);
        assert_eq!(packet.payload(), b"hello world");
        assert_eq!(packet.seq_nr(), 2);
        assert_eq!(packet.ack_nr(), 123);

        // Send back the state packet
        let mut p = Packet::state();
        p.set_connection_id(CONNECTION_ID);
        p.set_seq_nr(123); // Don't inc seq nr
        p.set_ack_nr(2);

        m.send_to(p, &addr);

        // No further packets sent on the socket
        m.assert_quiescence(1_000);
    });

    println!("~~~~~~~~~~~~~");

    th.join().unwrap();
}

fn sleep(ms: u64) {
    use std::thread;
    use std::time::Duration;
    thread::sleep(Duration::from_millis(ms));
}
