//! Signal core.
//!
//! Shape mirrors the ST side: every signal is `_Signal`-ish metadata plus a
//! typed value with lock/clamp semantics on the app-facing setter. Signals
//! are cheap `Rc` handles — declare them as struct fields, clone them freely,
//! register the root once and every leaf derives its dotted name from the
//! field path (the Rust replacement for `{attribute 'instance-path'}`).
//!
//! The scalar signals are one generic [`PmSignal<T>`] over the [`Value`]
//! trait; `PmBool`/`PmI32`/… are aliases. [`PmString`] stays its own type
//! (heap value, `&str` API, fixed 31-byte wire field).
//!
//! Lock semantics (same as ST): `locked == true` means the local app owns the
//! value; `set()` writes (clamped) only while locked. The *managers* decide
//! when to call the raw inbound appliers (`value_from_bytes`), so a remote
//! override lands regardless of lock — that gating lives in NetworkManager,
//! not here, exactly like the ST split.
//!
//! Deliberate deviations from ST, flagged for review:
//! * `value_from_text` (save/recording load) routes through the clamped
//!   setter — closes the P1-13 clamp/lock bypass instead of porting it.
//! * Wire strings: 31-byte field, always NUL-terminated (fixes stale tails,
//!   stays compatible with readers that stop at NUL or at 31).
//! * No `unit` metadata: units live in the field name (`cur_a`, `sw_lo_pm`)
//!   and so flow through the net schema and recordings automatically.
//! * No TIME/`PmDur` signal: durations are plain integers (ms). Pick the
//!   integer signal that fits and do `Duration::from_*(..)` math host-side.

use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::fmt::Write as _;

use crate::clock;

pub const UNBOUND: u16 = 0xFFFF;

// ---------------------------------------------------------------- cursors

pub struct WCursor<'a> {
    pub buf: &'a mut [u8],
    pub off: usize,
    pub ovf: bool,
}

impl<'a> WCursor<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        WCursor { buf, off: 0, ovf: false }
    }
    pub fn put(&mut self, bytes: &[u8]) {
        let end = self.off + bytes.len();
        if end > self.buf.len() {
            self.ovf = true;
            return;
        }
        self.buf[self.off..end].copy_from_slice(bytes);
        self.off = end;
    }
}

pub struct RCursor<'a> {
    pub buf: &'a [u8],
    pub off: usize,
}

impl<'a> RCursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        RCursor { buf, off: 0 }
    }
    /// Take exactly `n` bytes, or None on a short/garbled datagram
    /// (the ST "don't apply" guard).
    pub fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.off.checked_add(n)?;
        if end > self.buf.len() {
            return None;
        }
        let s = &self.buf[self.off..end];
        self.off = end;
        Some(s)
    }
}

// ------------------------------------------------------------------- meta

/// Per-signal bookkeeping, interior-mutable so managers can poke it through
/// shared handles. Fields are public; `name()`/`node()` exist only as
/// cloning conveniences for the hot comparisons.
#[derive(Debug)]
pub struct Meta {
    pub name: RefCell<String>,
    pub node: RefCell<String>,
    pub locked: Cell<bool>,
    pub save: Cell<bool>,
    /// This bool is a fault (set by `PmFault`): advertised as
    /// [`WireType::Fault`] so fault tables need no declaring code.
    pub fault: Cell<bool>,
    pub net_packet: Cell<u16>,
    pub net_offset: Cell<u32>,
    /// When the value last changed hands, ambient-clock ms (0 = never
    /// written). Stamped by *every* write path — app `set`, `set_raw`,
    /// inbound net apply, text load — so one field answers "how stale is
    /// this signal" for local and subscribed values alike.
    pub last_write_ms: Cell<u64>,
}

impl Default for Meta {
    fn default() -> Self {
        Meta {
            name: RefCell::new(String::new()),
            node: RefCell::new(String::new()),
            locked: Cell::new(true), // app owns by default, like ST
            save: Cell::new(false),
            fault: Cell::new(false),
            net_packet: Cell::new(UNBOUND),
            net_offset: Cell::new(0),
            last_write_ms: Cell::new(0),
        }
    }
}

impl Meta {
    pub fn name(&self) -> String {
        self.name.borrow().clone()
    }
    pub fn node(&self) -> String {
        self.node.borrow().clone()
    }
    pub fn resolved(&self) -> bool {
        self.net_packet.get() != UNBOUND
    }
}

// --------------------------------------------------------------- the trait

/// Object-safe view the managers hold (`Rc<dyn AnySignal>`).
pub trait AnySignal {
    fn meta(&self) -> &Meta;
    fn byte_size(&self) -> usize;
    /// Type tag advertised in the net schema so tools can decode payloads.
    fn wire_type(&self) -> WireType;
    fn value_to_bytes(&self, w: &mut WCursor);
    /// Raw inbound apply — no lock/clamp; the caller (manager) is the gate.
    fn value_from_bytes(&self, r: &mut RCursor) -> bool;
    fn value_to_text(&self, out: &mut String);
    /// Load-path apply — routes through the app setter (clamps + lock).
    fn value_from_text(&self, raw: &str);
    /// Human metadata as one string — `"[0.000..1.000] [Map]"`: configured
    /// bounds, then the map/text-list name (type-derived fallbacks like the
    /// bool map included). Empty when the signal has neither. Save-file
    /// lines, recording metadata rows, and tool displays all share this.
    fn metadata_text(&self) -> String;
    /// The schema entry's metadata block: `[flags]` then, per flag, typed
    /// `lo`/`hi` bytes ([`SCHEMA_META_BOUNDS`], only when configured
    /// tighter than the type's range) and the text-list name
    /// ([`SCHEMA_META_MAP`], only when explicitly set — type-derived
    /// fallbacks are synthesized by the receiver, not shipped).
    fn schema_meta_to_bytes(&self, w: &mut WCursor);
    /// The inverse: adopt a schema entry's metadata block as this signal's
    /// own bounds/text-list — how a shadow signal takes on its remote
    /// original's configuration. Metadata absent from the block resets to
    /// the type's defaults (it isn't layout: the owner may drop it without
    /// an epoch bump). Default no-op for types that carry none.
    fn schema_meta_from_bytes(&self, _meta: &[u8]) {}
    /// Current value as display text (empty when the type can't render).
    fn value_text(&self) -> String {
        let mut s = String::new();
        self.value_to_text(&mut s);
        s
    }
}

/// Compose [`AnySignal::metadata_text`] from a signal's bounds text
/// (`"0.000..1.000"` style) and map name. The bool fallback ([`BOOL_META`])
/// arrives pre-bracketed; bare map names get their brackets here.
pub(crate) fn metadata_from_parts(bounds: Option<String>, map: &str) -> String {
    let mut s = String::new();
    if let Some(b) = bounds {
        s.push('[');
        s.push_str(&b);
        s.push(']');
    }
    if !map.is_empty() {
        if !s.is_empty() {
            s.push(' ');
        }
        if map.starts_with('[') {
            s.push_str(map);
        } else {
            s.push('[');
            s.push_str(map);
            s.push(']');
        }
    }
    s
}

/// Implemented by every signal and every `pm_group!` struct.
pub trait Register {
    fn register(&self, r: &mut Registrar);
}

/// Chainable node stamping for anything that registers — replaces the ST
/// `node_io`/`node_cfg` constructor plumbing. `group.node("io")` stamps every
/// signal underneath; `.cfg_node("hmi")` re-stamps just the `.save()`
/// (configuration) signals.
pub trait Stamp: Register + Sized {
    fn node(self, n: &str) -> Self {
        for s in &Registrar::collect(&self).signals {
            *s.meta().node.borrow_mut() = n.to_string();
        }
        self
    }
    fn cfg_node(self, n: &str) -> Self {
        for s in &Registrar::collect(&self).signals {
            if s.meta().save.get() {
                *s.meta().node.borrow_mut() = n.to_string();
            }
        }
        self
    }
}

impl<T: Register> Stamp for T {}

pub struct Registrar {
    path: Vec<String>,
    pub signals: Vec<Rc<dyn AnySignal>>,
    pub faults: Vec<crate::fault::PmFault>,
}

impl Registrar {
    pub fn collect(root: &impl Register) -> Self {
        let mut r = Registrar { path: Vec::new(), signals: Vec::new(), faults: Vec::new() };
        root.register(&mut r);
        r
    }
    pub fn enter(&mut self, seg: &str) {
        self.path.push(seg.to_string());
    }
    pub fn leave(&mut self) {
        self.path.pop();
    }
    pub fn leaf(&mut self, sig: Rc<dyn AnySignal>) {
        *sig.meta().name.borrow_mut() = self.path.join(".");
        self.signals.push(sig);
    }
    /// Register `node` under the path segment `seg` — the enter/register/
    /// leave triple every composite `Register` impl needs.
    pub fn child(&mut self, seg: &str, node: &impl Register) {
        self.enter(seg);
        node.register(self);
        self.leave();
    }
    pub fn fault(&mut self, f: &crate::fault::PmFault) {
        self.faults.push(f.clone());
    }
}

// ---------------------------------------------------------------- helpers

fn fmt_real(v: f32, out: &mut String) {
    // Fixed 3 decimals like the ST recording format (csv_write_lreal(_, 3)).
    // core::fmt rounds the true value; ST rounds after a scale-multiply —
    // close enough now that byte-for-byte parity is no longer a goal.
    let _ = write!(out, "{v:.3}");
}

// ------------------------------------------------------------------ value

/// Wire type tag carried in schema advertisements, so a tool on the segment
/// can decode payloads without the declaring code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum WireType {
    Bool = 0,
    I32 = 1,
    I64 = 2,
    U64 = 3,
    F32 = 4,
    Str = 5,
    /// Bool-shaped, but the signal is a fault — lets a tool on the segment
    /// build a fault table with no declaring code.
    Fault = 6,
}

impl WireType {
    /// Wire size in bytes — fully determined by the type, which is why the
    /// schema carries no size field.
    pub const fn byte_size(self) -> usize {
        match self {
            WireType::Bool | WireType::Fault => 1,
            WireType::I32 | WireType::F32 => 4,
            WireType::I64 | WireType::U64 => 8,
            WireType::Str => STRING_WIRE_BYTES,
        }
    }

    /// Construct a signal of this wire type — how a tool (the Monitor)
    /// materializes a *shadow* of a remote signal from nothing but the
    /// schema: the shadow is the same object the owner has, so every
    /// signal mechanism (recording, text, metadata) works on it unchanged.
    pub fn make_signal(self) -> Rc<dyn AnySignal> {
        match self {
            WireType::Bool => PmBool::new().0,
            WireType::I32 => PmI32::new().0,
            WireType::I64 => PmI64::new().0,
            WireType::U64 => PmU64::new().0,
            WireType::F32 => PmF32::new().0,
            WireType::Str => PmString::new().0,
            WireType::Fault => {
                let s = PmBool::new();
                s.0.meta.fault.set(true); // advertises (and displays) as Fault
                s.0
            }
        }
    }

    /// Display name, for tools rendering schema info (recording metadata
    /// rows, unlock prompts).
    pub fn text(self) -> &'static str {
        match self {
            WireType::Bool => "bool",
            WireType::I32 => "i32",
            WireType::I64 => "i64",
            WireType::U64 => "u64",
            WireType::F32 => "f32",
            WireType::Str => "str",
            WireType::Fault => "fault",
        }
    }

    pub fn from_u8(v: u8) -> Option<WireType> {
        Some(match v {
            0 => WireType::Bool,
            1 => WireType::I32,
            2 => WireType::I64,
            3 => WireType::U64,
            4 => WireType::F32,
            5 => WireType::Str,
            6 => WireType::Fault,
            _ => return None,
        })
    }
}

/// Decode a wire value of `ty` from `bytes` into display text — the same
/// formatting the owning signal's `value_to_text` produces.
pub fn wire_value_to_text(ty: WireType, bytes: &[u8], out: &mut String) {
    fn dec<T: Value>(bytes: &[u8], out: &mut String) {
        if let Some(v) = T::from_bytes(&mut RCursor::new(bytes)) {
            v.to_text(out);
        }
    }
    match ty {
        WireType::Bool | WireType::Fault => dec::<bool>(bytes, out),
        WireType::I32 => dec::<i32>(bytes, out),
        WireType::I64 => dec::<i64>(bytes, out),
        WireType::U64 => dec::<u64>(bytes, out),
        WireType::F32 => dec::<f32>(bytes, out),
        WireType::Str => {
            let end = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
            out.push_str(&String::from_utf8_lossy(&bytes[..end]));
        }
    }
}

/// Encode display text into a wire value of `ty` (its size follows from the
/// type) — the inverse of [`wire_value_to_text`], for tools that write
/// values they only know from the schema (the unlock prompt). `None` when
/// the text doesn't parse. Bool text follows the signal convention: `"1"`
/// is true, anything else false.
pub fn wire_value_from_text(ty: WireType, text: &str) -> Option<Vec<u8>> {
    fn enc<T: Value>(text: &str) -> Option<Vec<u8>> {
        let mut out = alloc::vec![0u8; T::TY.byte_size()];
        T::from_text(text)?.to_bytes(&mut WCursor::new(&mut out));
        Some(out)
    }
    match ty {
        WireType::Bool | WireType::Fault => enc::<bool>(text),
        WireType::I32 => enc::<i32>(text),
        WireType::I64 => enc::<i64>(text),
        WireType::U64 => enc::<u64>(text),
        WireType::F32 => enc::<f32>(text),
        WireType::Str => {
            // Fixed field, always NUL-terminated: text capped to the field
            // minus terminator, tail zeroed — the shape PmString sends.
            let mut out = alloc::vec![0u8; STRING_WIRE_BYTES];
            let n = text.len().min(STRING_WIRE_BYTES - 1);
            out[..n].copy_from_slice(&text.as_bytes()[..n]);
            Some(out)
        }
    }
}

// ------------------------------------------------------- schema metadata

/// Schema-entry metadata flag: typed `lo`/`hi` bounds follow.
pub const SCHEMA_META_BOUNDS: u8 = 1 << 0;
/// Schema-entry metadata flag: a text-list/map name fills the rest.
pub const SCHEMA_META_MAP: u8 = 1 << 1;

/// The bool fallback map — type-derived, so it never travels on the wire;
/// both ends synthesize it (`Value::META` here, tools for remote bools).
pub const BOOL_META: &str = "[0=Inactive; 1=Active]";

/// A scalar a [`PmSignal`] can carry: LE wire encoding, recording text,
/// clamp bounds. Durations are deliberately not a type here — model them as
/// plain integer ms on whichever width fits.
pub trait Value: Copy + Default + PartialOrd + 'static {
    const TY: WireType;
    const LO: Self;
    const HI: Self;
    /// Fallback for `metadata_text` when no text list is set.
    const META: &'static str = "";
    fn to_bytes(self, w: &mut WCursor);
    fn from_bytes(r: &mut RCursor) -> Option<Self>;
    fn to_text(self, out: &mut String);
    fn from_text(raw: &str) -> Option<Self>;
    fn clamped(self, lo: Self, hi: Self) -> Self {
        // NaN-safe: both comparisons false → value passes through.
        if self < lo {
            lo
        } else if self > hi {
            hi
        } else {
            self
        }
    }
}

impl Value for bool {
    const TY: WireType = WireType::Bool;
    const LO: Self = false;
    const HI: Self = true;
    const META: &'static str = BOOL_META;
    fn to_bytes(self, w: &mut WCursor) {
        w.put(&[self as u8]);
    }
    fn from_bytes(r: &mut RCursor) -> Option<Self> {
        r.take(1).map(|b| b[0] != 0)
    }
    fn to_text(self, out: &mut String) {
        out.push(if self { '1' } else { '0' });
    }
    fn from_text(raw: &str) -> Option<Self> {
        Some(raw.trim() == "1")
    }
}

macro_rules! num_value {
    ($($T:ty => $ty:expr, $disp:expr;)+) => {$(
        impl Value for $T {
            const TY: WireType = $ty;
            const LO: Self = <$T>::MIN;
            const HI: Self = <$T>::MAX;
            fn to_bytes(self, w: &mut WCursor) {
                w.put(&self.to_le_bytes());
            }
            fn from_bytes(r: &mut RCursor) -> Option<Self> {
                r.take($ty.byte_size()).map(|b| <$T>::from_le_bytes(b.try_into().unwrap()))
            }
            fn to_text(self, out: &mut String) {
                let f: fn($T, &mut String) = $disp;
                f(self, out);
            }
            fn from_text(raw: &str) -> Option<Self> {
                raw.trim().parse().ok()
            }
        }
    )+};
}

num_value! {
    i32 => WireType::I32, |v, out| { let _ = write!(out, "{v}"); };
    i64 => WireType::I64, |v, out| { let _ = write!(out, "{v}"); };
    u64 => WireType::U64, |v, out| { let _ = write!(out, "{v}"); };
    f32 => WireType::F32, fmt_real;
}

// --------------------------------------------------------------- PmSignal

pub(crate) struct PmSignalInner<T> {
    pub meta: Meta,
    pub value: Cell<T>,
    pub lo: Cell<T>,
    pub hi: Cell<T>,
    /// Text-list name for enum-flavored ints (ST `text_list` attribute).
    pub text_list: RefCell<String>,
}

pub struct PmSignal<T>(pub(crate) Rc<PmSignalInner<T>>);

impl<T> Clone for PmSignal<T> {
    fn clone(&self) -> Self {
        PmSignal(self.0.clone())
    }
}

pub type PmBool = PmSignal<bool>;
pub type PmI32 = PmSignal<i32>;
pub type PmI64 = PmSignal<i64>;
pub type PmU64 = PmSignal<u64>;
pub type PmF32 = PmSignal<f32>;

impl<T: Value> PmSignal<T> {
    pub fn new() -> Self {
        PmSignal(Rc::new(PmSignalInner {
            meta: Meta::default(),
            value: Cell::new(T::default()),
            lo: Cell::new(T::LO),
            hi: Cell::new(T::HI),
            text_list: RefCell::new(String::new()),
        }))
    }
    pub fn init(self, v: T) -> Self {
        self.0.value.set(v);
        self
    }
    pub fn save(self) -> Self {
        self.0.meta.save.set(true);
        self
    }
    pub fn lo(self, v: T) -> Self {
        self.0.lo.set(v);
        self
    }
    pub fn hi(self, v: T) -> Self {
        self.0.hi.set(v);
        self
    }
    pub fn range(self, lo: T, hi: T) -> Self {
        self.0.lo.set(lo);
        self.0.hi.set(hi);
        self
    }
    pub fn text_list(self, t: &str) -> Self {
        *self.0.text_list.borrow_mut() = t.to_string();
        self
    }
    pub fn val(&self) -> T {
        self.0.value.get()
    }
    /// App-facing write: lock gate + lo/hi clamp (`LIMIT`).
    pub fn set(&self, v: T) {
        if self.0.meta.locked.get() {
            self.0.value.set(v.clamped(self.0.lo.get(), self.0.hi.get()));
            self.0.meta.last_write_ms.set(clock::now_ms());
        }
    }
    /// Raw write: bypasses lock and clamp, like an inbound network apply.
    pub fn set_raw(&self, v: T) {
        self.0.value.set(v);
        self.0.meta.last_write_ms.set(clock::now_ms());
    }
    pub fn meta(&self) -> &Meta {
        &self.0.meta
    }
}

impl<T: Value> Default for PmSignal<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Value> AnySignal for PmSignalInner<T> {
    fn meta(&self) -> &Meta {
        &self.meta
    }
    fn byte_size(&self) -> usize {
        T::TY.byte_size()
    }
    fn wire_type(&self) -> WireType {
        if self.meta.fault.get() { WireType::Fault } else { T::TY }
    }
    fn value_to_bytes(&self, w: &mut WCursor) {
        self.value.get().to_bytes(w);
    }
    fn value_from_bytes(&self, r: &mut RCursor) -> bool {
        match T::from_bytes(r) {
            Some(v) => {
                self.value.set(v);
                self.meta.last_write_ms.set(clock::now_ms());
                true
            }
            None => false,
        }
    }
    fn value_to_text(&self, out: &mut String) {
        self.value.get().to_text(out);
    }
    fn value_from_text(&self, raw: &str) {
        if self.meta.locked.get()
            && let Some(v) = T::from_text(raw)
        {
            self.value.set(v.clamped(self.lo.get(), self.hi.get()));
            self.meta.last_write_ms.set(clock::now_ms());
        }
    }
    fn metadata_text(&self) -> String {
        let bounds = (self.lo.get() != T::LO || self.hi.get() != T::HI).then(|| {
            let mut b = String::new();
            self.lo.get().to_text(&mut b);
            b.push_str("..");
            self.hi.get().to_text(&mut b);
            b
        });
        let t = self.text_list.borrow();
        let map = if t.is_empty() { T::META } else { t.as_str() };
        metadata_from_parts(bounds, map)
    }
    fn schema_meta_to_bytes(&self, w: &mut WCursor) {
        let bounded = self.lo.get() != T::LO || self.hi.get() != T::HI;
        let map = self.text_list.borrow();
        let mut flags = 0u8;
        if bounded {
            flags |= SCHEMA_META_BOUNDS;
        }
        if !map.is_empty() {
            flags |= SCHEMA_META_MAP;
        }
        w.put(&[flags]);
        if bounded {
            self.lo.get().to_bytes(w);
            self.hi.get().to_bytes(w);
        }
        w.put(map.as_bytes());
    }
    fn schema_meta_from_bytes(&self, meta: &[u8]) {
        let (flags, mut rest) = match meta.split_first() {
            Some((&flags, rest)) => (flags, rest),
            None => (0, &[][..]),
        };
        let mut bounded = false;
        if flags & SCHEMA_META_BOUNDS != 0 {
            let mut r = RCursor::new(rest);
            if let Some(lo) = T::from_bytes(&mut r)
                && let Some(hi) = T::from_bytes(&mut r)
            {
                self.lo.set(lo);
                self.hi.set(hi);
                rest = &rest[r.off..];
                bounded = true;
            }
        }
        if !bounded {
            self.lo.set(T::LO);
            self.hi.set(T::HI);
        }
        let mut map = self.text_list.borrow_mut();
        map.clear();
        if flags & SCHEMA_META_MAP != 0 {
            map.push_str(&String::from_utf8_lossy(rest));
        }
    }
}

impl<T: Value> Register for PmSignal<T> {
    fn register(&self, r: &mut Registrar) {
        r.leaf(self.0.clone());
    }
}

// ---------------------------------------------------------- PmString

pub const STRING_WIRE_BYTES: usize = 31; // ST parity: STRING(30) + NUL

pub(crate) struct PmStringInner {
    pub meta: Meta,
    pub value: RefCell<String>,
}

#[derive(Clone)]
pub struct PmString(pub(crate) Rc<PmStringInner>);

impl PmString {
    pub fn new() -> Self {
        PmString(Rc::new(PmStringInner { meta: Meta::default(), value: RefCell::new(String::new()) }))
    }
    pub fn init(self, v: &str) -> Self {
        *self.0.value.borrow_mut() = v.to_string();
        self
    }
    pub fn save(self) -> Self {
        self.0.meta.save.set(true);
        self
    }
    pub fn val(&self) -> String {
        self.0.value.borrow().clone()
    }
    pub fn set(&self, v: &str) {
        if self.0.meta.locked.get() {
            *self.0.value.borrow_mut() = v.to_string();
            self.0.meta.last_write_ms.set(clock::now_ms());
        }
    }
    /// Raw write: bypasses the lock, like an inbound network apply.
    pub fn set_raw(&self, v: &str) {
        *self.0.value.borrow_mut() = v.to_string();
        self.0.meta.last_write_ms.set(clock::now_ms());
    }
    pub fn meta(&self) -> &Meta {
        &self.0.meta
    }
}

impl Default for PmString {
    fn default() -> Self {
        Self::new()
    }
}

impl AnySignal for PmStringInner {
    fn meta(&self) -> &Meta {
        &self.meta
    }
    fn byte_size(&self) -> usize {
        STRING_WIRE_BYTES
    }
    fn wire_type(&self) -> WireType {
        WireType::Str
    }
    fn value_to_bytes(&self, w: &mut WCursor) {
        let mut field = [0u8; STRING_WIRE_BYTES];
        let v = self.value.borrow();
        let bytes = v.as_bytes();
        // Cap at 30 so the field always carries a terminator (stale-tail fix);
        // trim to a char boundary so we never split UTF-8.
        let mut n = bytes.len().min(STRING_WIRE_BYTES - 1);
        while n > 0 && !v.is_char_boundary(n) {
            n -= 1;
        }
        field[..n].copy_from_slice(&bytes[..n]);
        w.put(&field);
    }
    fn value_from_bytes(&self, r: &mut RCursor) -> bool {
        match r.take(STRING_WIRE_BYTES) {
            Some(b) => {
                let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                *self.value.borrow_mut() = String::from_utf8_lossy(&b[..end]).into_owned();
                self.meta.last_write_ms.set(clock::now_ms());
                true
            }
            None => false,
        }
    }
    fn value_to_text(&self, out: &mut String) {
        out.push_str(&self.value.borrow());
    }
    fn value_from_text(&self, raw: &str) {
        if self.meta.locked.get() {
            *self.value.borrow_mut() = raw.to_string();
            self.meta.last_write_ms.set(clock::now_ms());
        }
    }
    fn metadata_text(&self) -> String {
        String::new()
    }
    fn schema_meta_to_bytes(&self, w: &mut WCursor) {
        w.put(&[0]); // strings carry no bounds or map
    }
}

impl Register for PmString {
    fn register(&self, r: &mut Registrar) {
        r.leaf(self.0.clone());
    }
}
