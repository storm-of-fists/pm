//! Micro-bench suite: ns/op for every kernel hot path. Run release or
//! the numbers are fiction:
//!
//!     cargo run --release -p pm --example bench
//!
//! Companion to `taskbench` (task dispatch + pool access patterns) and
//! `sim` (the end-to-end 100k-entity loop). These are the numbers to
//! eyeball after kernel changes; threshold-gated regression checks can
//! grow out of them later.

use std::hint::black_box;
use std::time::Instant;

use pm::{NetClient, NetServer, Pm, Predictor, SpatialGrid, pool_mirror, vec2};

#[derive(Clone, Copy, PartialEq, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct P {
    x: f32,
    y: f32,
}

#[derive(Clone, Copy, PartialEq, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct V {
    x: f32,
    y: f32,
}

#[derive(Clone, Copy, PartialEq, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Cmd {
    dx: f32,
}

fn time(label: &str, ops: u64, f: impl FnOnce()) {
    let t = Instant::now();
    f();
    let ns = t.elapsed().as_nanos() as f64 / ops as f64;
    println!("  {label:<42} {ns:>9.2} ns/op");
}

const N: u32 = 100_000;

fn pools() {
    println!("-- pools ({N} entities) --");
    let mut pm = Pm::new();
    let pos = pm.pool::<P>("pos");
    let vel = pm.pool::<V>("vel");
    // 100k exceeds the 65k-per-peer index budget; spread across two.
    let ids: Vec<_> = (0..N).map(|i| pm.id_add_for((i / 60_000) as u8)).collect();

    time("add", N as u64, || {
        let mut p = pos.borrow_mut();
        for &id in &ids {
            p.add(id, P::default());
        }
    });
    {
        let mut v = vel.borrow_mut();
        for (i, &id) in ids.iter().enumerate() {
            v.add(id, V { x: (i % 7) as f32, y: (i % 3) as f32 });
        }
    }
    time("get (sparse lookup + gen check)", N as u64, || {
        let p = pos.borrow();
        let mut acc = 0.0f32;
        for &id in &ids {
            acc += p.get(id).unwrap().x;
        }
        black_box(acc);
    });
    time("iter (dense read)", N as u64, || {
        let p = pos.borrow();
        let mut acc = 0.0f32;
        for (_, v) in p.iter() {
            acc += v.x;
        }
        black_box(acc);
    });
    time("iter_mut (write + change stamp)", N as u64, || {
        for (_, mut v) in pos.borrow_mut().iter_mut() {
            v.x += 1.0;
        }
    });
    time("each_with join (vel -> pos, write)", N as u64, || {
        vel.borrow_mut().each_with(&mut pos.borrow_mut(), |_, v, mut p| {
            p.x += v.x;
            p.y += v.y;
        });
    });
    time("iter_with join (read)", N as u64, || {
        let p = pos.borrow();
        let v = vel.borrow();
        let mut acc = 0.0f32;
        for (_, a, b) in p.iter_with(&v) {
            acc += a.x + b.x;
        }
        black_box(acc);
    });
    // Data-dependent predicate (always true here, but the optimizer
    // can't know that) so the scan + loads actually happen.
    time("retain (keep all — scan cost)", N as u64, || {
        pos.borrow_mut().retain(|_, v| v.x > -1.0);
    });
}

fn id_lifecycle() {
    println!("-- id lifecycle (add + remove + end-of-tick flush) --");
    let mut pm = Pm::new();
    let pool = pm.pool::<P>("p");
    const M: u32 = 50_000;
    time("spawn + despawn cycle", M as u64, || {
        let ids: Vec<_> = (0..M)
            .map(|_| {
                let id = pm.id_add();
                pool.borrow_mut().add(id, P::default());
                id
            })
            .collect();
        for id in ids {
            pm.id_remove(id);
        }
        pm.loop_once(1.0 / 60.0); // flush
    });
}

fn spatial() {
    println!("-- spatial grid (10k entities, 2000x2000, cell 64) --");
    let mut pm = Pm::new();
    let mut grid = SpatialGrid::new(2000.0, 2000.0, 64.0);
    let ids: Vec<_> = (0..10_000)
        .map(|i| {
            let id = pm.id_add();
            (id, vec2((i % 100) as f32 * 20.0, (i / 100) as f32 * 20.0))
        })
        .collect();
    time("insert", 10_000, || {
        for &(id, p) in &ids {
            grid.insert(id, p);
        }
    });
    time("query r=64 (per query)", 10_000, || {
        let mut hits = 0u32;
        for &(_, p) in &ids {
            grid.query(p, 64.0, |_, _| hits += 1);
        }
        black_box(hits);
    });
}

fn net_sync() {
    println!("-- net sync (10k entities, one peer) --");
    const M: u32 = 10_000;
    let mut spm = Pm::new();
    let s_pos = spm.pool::<P>("pos");
    let mut net = NetServer::new(&mut spm);
    net.pool_sync("pos", &s_pos);
    net.peer_add(1);
    for i in 0..M {
        let id = spm.id_add();
        s_pos.borrow_mut().add(id, P { x: i as f32, y: 0.0 });
    }
    spm.loop_once(1.0 / 60.0);

    let mut cpm = Pm::new();
    let c_pos = cpm.pool::<P>("pos");
    let mut cnet = NetClient::new();
    cnet.pool_sync("pos", &c_pos);

    let mut snap = Vec::new();
    time("pack, all dirty (uncapped)", M as u64, || {
        snap = net.snapshot(&spm, 1).unwrap();
    });
    println!("    ({} KiB on the wire)", snap.len() / 1024);
    time("apply, all entries", M as u64, || {
        cnet.apply(&mut cpm, &snap).unwrap();
    });
    let label = u32::from_le_bytes(snap[0..4].try_into().unwrap());
    net.ack(1, label);
    spm.loop_once(1.0 / 60.0);
    // The documented known limit: a converged pool still costs a full
    // per-peer scan every net tick. This is that scan, per entity.
    time("pack, converged (idle scan)", M as u64, || {
        black_box(net.snapshot_budgeted(&spm, 1, 1200));
    });
}

fn predictor() {
    println!("-- predictor (rewind-replay ring) --");
    let step = |s: &mut P, c: Cmd| {
        s.x += c.dx * (1.0 / 60.0);
    };
    let err = |a: &P, b: &P| (a.x - b.x).abs();
    let mut pred: Predictor<P, Cmd> = Predictor::default();
    pred.reconcile(P::default(), 0, step, err, 1e-6); // seed
    const ROUNDS: u32 = 10_000;
    let mut seq = 0u32;
    time("predict (record + step)", ROUNDS as u64, || {
        for _ in 0..ROUNDS {
            seq += 1;
            pred.predict(seq, Cmd { dx: 1.0 }, step);
        }
        black_box(pred.state());
    });
    // Each round: 120 fresh inputs, then a diverging echo 120 back —
    // every reconcile rewinds and replays 120 commands.
    time("reconcile w/ 120-step replay", 100, || {
        for i in 0..100u32 {
            for _ in 0..120 {
                seq += 1;
                pred.predict(seq, Cmd { dx: 1.0 }, step);
            }
            let auth = P { x: i as f32 * -10.0, y: 0.0 };
            black_box(pred.reconcile(auth, seq - 120, step, err, 1e-6));
        }
        black_box(pred.state());
    });
}

fn mirror() {
    println!("-- pool_mirror (10k entities, per full mirror pass) --");
    let mut pm = Pm::new();
    let auth = pm.pool::<P>("auth");
    let draw = pm.pool::<P>("draw");
    for i in 0..10_000 {
        let id = pm.id_add();
        auth.borrow_mut().add(id, P { x: i as f32, y: 0.0 });
    }
    time("mirror + blend", 10_000 * 100, || {
        for _ in 0..100 {
            pool_mirror(&auth, &draw, |_, d, a: &P| P { x: d.x + (a.x - d.x) * 0.15, y: a.y });
        }
    });
}

fn main() {
    pools();
    id_lifecycle();
    spatial();
    net_sync();
    predictor();
    mirror();
    println!("(task dispatch overhead: see `taskbench`; end-to-end loop: see `sim`)");
}
