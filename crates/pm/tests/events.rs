//! Typed client→server events: the encode/decode/tag layer over the raw
//! reliable channel. The transport itself (outbox → wire → received) is
//! covered by the quic loopback tests; here we verify that a `PmClient`'s
//! `EventTx` and a `PmServer`'s `EventRx` agree on the wire tag, round-trip
//! the pod faithfully, carry the sender peer, and ignore other channels.

use pm::{Outbox, Pm, ServerEvents};

#[derive(Clone, Copy, Default, PartialEq, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Honk {
    freq: u32,
}

#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Ready {
    _pad: u32,
}

/// Move whatever the client queued in its outbox into the server's
/// received-events single, stamped as coming from `peer` — standing in for
/// the QUIC hop the loopback tests already exercise.
fn deliver(client: &mut Pm, server: &mut Pm, peer: u8) {
    let frames = client.single::<Outbox>("net.out").get_mut().drain();
    let ev = server.single::<ServerEvents>("net.events");
    let mut ev = ev.get_mut();
    for (ty, payload) in frames {
        ev.0.push((peer, ty, payload));
    }
}

#[test]
fn typed_events_round_trip_client_to_server() {
    let mut client = Pm::client("127.0.0.1:0", 60.0);
    let mut server = Pm::server("127.0.0.1:0");
    let honk_tx = client.event::<Honk>("honk");
    let honk_rx = server.event::<Honk>("honk");

    honk_tx.send(Honk { freq: 440 });
    honk_tx.send(Honk { freq: 880 });
    deliver(&mut client, &mut server, 7);

    // Both honks arrive, in order, tagged with the sender peer, byte-exact.
    assert_eq!(
        honk_rx.drain(),
        vec![(7, Honk { freq: 440 }), (7, Honk { freq: 880 })]
    );
}

#[test]
fn rx_ignores_other_event_channels() {
    let mut client = Pm::client("127.0.0.1:0", 60.0);
    let mut server = Pm::server("127.0.0.1:0");
    // Two channels registered; the server only listens for honks.
    let honk_tx = client.event::<Honk>("honk");
    let ready_tx = client.event::<Ready>("ready");
    let honk_rx = server.event::<Honk>("honk");

    ready_tx.send(Ready::default());
    honk_tx.send(Honk { freq: 100 });
    deliver(&mut client, &mut server, 3);

    // The "ready" frame shares the outbox but a different tag, so the honk
    // receiver must not pick it up.
    assert_eq!(honk_rx.drain(), vec![(3, Honk { freq: 100 })]);
}

#[test]
fn same_name_registers_idempotently() {
    // Re-registering the same channel (e.g. both a setup helper and a task
    // grab it) shares the tag rather than tripping the collision guard.
    let mut client = Pm::client("127.0.0.1:0", 60.0);
    let mut server = Pm::server("127.0.0.1:0");
    let a = client.event::<Honk>("honk");
    let b = client.event::<Honk>("honk"); // no panic
    let rx = server.event::<Honk>("honk");

    a.send(Honk { freq: 1 });
    b.send(Honk { freq: 2 });
    deliver(&mut client, &mut server, 5);
    assert_eq!(rx.drain(), vec![(5, Honk { freq: 1 }), (5, Honk { freq: 2 })]);
}
