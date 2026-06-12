//! Authoritative drive server: headless, fixed-dt, one input per tick
//! per peer (command-frame model), budget-rotated snapshots. The same
//! netcode shape as the 2D demo — the simulation just gets rendered in
//! 3D on the other side.

use std::collections::{HashMap, VecDeque};

use pm::{Id, NetServer, Pm, QuicServer};

use crate::common::*;

#[derive(Default)]
struct Inbox(HashMap<u8, VecDeque<(u32, Drive)>>);
#[derive(Default)]
struct LastCmd(HashMap<u8, (u32, Drive)>);
#[derive(Default)]
struct Garage(HashMap<u8, Id>);

pub fn run(quiet: bool) {
    let mut pm = Pm::new();
    let car = pm.pool::<Car>("car");
    let inbox = pm.single::<Inbox>("inbox");
    let last_cmd = pm.single::<LastCmd>("last_cmd");
    let garage = pm.single::<Garage>("garage");

    let mut net = NetServer::new(&mut pm);
    net.pool_sync("car", &car);
    let mut quic = QuicServer::bind(ADDR, &net.schema()).unwrap_or_else(|e| {
        eprintln!("cannot bind {ADDR}: {e}");
        eprintln!("(a previous drive may still be running: pkill -x drive)");
        std::process::exit(1);
    });
    if !quiet {
        eprintln!("drive server on {ADDR}");
    }

    pm.task_add("net", 5.0, {
        let car = car.clone();
        let inbox = inbox.clone();
        let last_cmd = last_cmd.clone();
        let garage = garage.clone();
        move |pm| {
            quic.pump();
            for p in quic.joined_drain() {
                net.peer_add(p);
                let id = pm.id_add();
                car.borrow_mut().add(id, spawn_car(p));
                garage.borrow_mut().0.insert(p, id);
                quic.event_send(p, EV_VEHICLE, &id.0.to_le_bytes());
                if !quiet {
                    eprintln!("[server] peer {p} joined");
                }
            }
            for p in quic.left_drain() {
                net.peer_remove(p);
                inbox.borrow_mut().0.remove(&p);
                last_cmd.borrow_mut().0.remove(&p);
                if let Some(id) = garage.borrow_mut().0.remove(&p) {
                    pm.id_remove(id);
                }
                if !quiet {
                    eprintln!("[server] peer {p} left");
                }
            }
            for (p, seq, bytes) in quic.inputs_drain() {
                if bytes.len() == size_of::<Drive>() {
                    inbox
                        .borrow_mut()
                        .0
                        .entry(p)
                        .or_default()
                        .push_back((seq, bytemuck::pod_read_unaligned(&bytes)));
                }
            }
            for (p, tick) in quic.acks_drain() {
                net.ack(p, tick);
            }
            for (&p, &(seq, _)) in &last_cmd.borrow().0 {
                net.input_processed(p, seq);
            }
            let peers: Vec<u8> = net.peers().collect();
            for p in peers {
                let budget = quic.snapshot_budget(p);
                if let Some(snap) = net.snapshot_budgeted(pm, p, budget) {
                    quic.snapshot_send(p, &snap);
                }
            }
            net.prune(pm);
        }
    });

    pm.task_add("drive", 30.0, {
        let car = car.clone();
        let inbox = inbox.clone();
        let last_cmd = last_cmd.clone();
        let garage = garage.clone();
        move |_pm| {
            let mut inbox = inbox.borrow_mut();
            let mut last_cmd = last_cmd.borrow_mut();
            let mut car = car.borrow_mut();
            for (&peer, &id) in &garage.borrow().0 {
                let q = inbox.0.entry(peer).or_default();
                // Bound queue-induced input latency to ~2 ticks.
                while q.len() > 2 {
                    let skipped = q.pop_front().unwrap();
                    last_cmd.0.insert(peer, skipped);
                }
                if let Some(next) = q.pop_front() {
                    last_cmd.0.insert(peer, next);
                }
                let (_, cmd) = last_cmd.0.get(&peer).copied().unwrap_or_default();
                if let Some(mut c) = car.get_mut(id) {
                    drive_step(&mut c, cmd, FIXED_DT);
                }
            }
        }
    });

    if !quiet {
        pm.task_add_every("status", 90.0, 5.0, {
            let car = car.clone();
            move |pm| {
                let cars = car.borrow();
                let speeds: Vec<String> =
                    cars.values().iter().map(|c| format!("{:.1}", c.speed)).collect();
                eprintln!("[server] t={} cars={} speeds=[{}]", pm.tick() / 60, cars.len(), speeds.join(", "));
            }
        });
    }

    pm.loop_rate = 60;
    pm.loop_run();
}
