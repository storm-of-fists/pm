//! Authoritative drive server. All transport plumbing lives in pm's net
//! module (`NetServer::serve`) — this file is pure gameplay: spawn a car
//! per peer, step each car with its command-frame input.

use std::collections::HashMap;

use pm::{Commands, Id, NetServer, PeerEvents, Pm, QuicServer, ServerOutbox};

use crate::common::*;

#[derive(Default)]
struct Garage(HashMap<u8, Id>);

pub fn run(quiet: bool) {
    let mut pm = Pm::new();
    let car = pm.pool::<Car>("car");
    let garage = pm.single::<Garage>("garage");

    let mut net = NetServer::new(&mut pm);
    net.pool_sync("car", &car);
    let quic = QuicServer::bind(ADDR, &net.schema()).unwrap_or_else(|e| {
        eprintln!("cannot bind {ADDR}: {e}");
        eprintln!("(a previous drive may still be running: pkill -x drive)");
        std::process::exit(1);
    });
    if !quiet {
        eprintln!("drive server on {ADDR}");
    }
    net.serve::<Drive>(&mut pm, quic);
    let peers = pm.single::<PeerEvents>("net.peers");
    let cmds = pm.single::<Commands<Drive>>("net.cmds");
    let out = pm.single::<ServerOutbox>("net.out");

    // Joins and leaves: a car per peer (prio 10 — after the net task).
    pm.task_add("roster", 10.0, 0.0, {
        let car = car.clone();
        let garage = garage.clone();
        move |pm| {
            for &p in &peers.borrow().joined {
                let id = pm.id_add();
                car.borrow_mut().add(id, spawn_car(p));
                garage.borrow_mut().0.insert(p, id);
                out.borrow_mut().send(p, EV_VEHICLE, &id.0.to_le_bytes());
                if !quiet {
                    eprintln!("[server] peer {p} joined");
                }
            }
            for &p in &peers.borrow().left {
                if let Some(id) = garage.borrow_mut().0.remove(&p) {
                    pm.id_remove(id);
                }
                if !quiet {
                    eprintln!("[server] peer {p} left");
                }
            }
        }
    });

    pm.task_add("drive", 30.0, 0.0, {
        let car = car.clone();
        let garage = garage.clone();
        move |_pm| {
            let mut cmds = cmds.borrow_mut();
            let mut car = car.borrow_mut();
            for (&peer, &id) in &garage.borrow().0 {
                // pop = command-frame consumption: one input per tick,
                // hold-when-dry, bounded skip-ahead. The consumed seq is
                // echoed automatically for prediction reconciliation.
                let cmd = cmds.pop(peer);
                if let Some(mut c) = car.get_mut(id) {
                    drive_step(&mut c, cmd, FIXED_DT);
                }
            }
        }
    });

    if !quiet {
        pm.task_add("status", 90.0, 5.0, {
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
