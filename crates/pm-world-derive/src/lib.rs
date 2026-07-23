//! `#[derive(pm::Wire)]` — compact wire representations for synced pods.
//!
//! The pool IS the wire format (see pm's design decisions), so bandwidth
//! work means compact pods. This derive keeps the game's struct ergonomic
//! (plain `f32` fields) while generating the small pod that actually rides
//! the wire: a `#[repr(C, packed)]` sibling struct named `<Name>Wire`, plus
//! the `pm::Wire` impl converting both ways. Register with
//! `Pm::wire_pool` instead of `sync_pool` and the repr is what snapshots
//! carry — the game never sees it.
//!
//! Per-field quantization via attribute; fields without one pass through
//! unchanged (and must themselves be `Pod`):
//!
//! ```ignore
//! #[derive(Clone, Copy, Pod, Zeroable, pm::Wire)]
//! #[repr(C)]
//! pub struct Hog {
//!     #[wire(i16, scale = 64.0)] // wire = round(x * 64), back = wire / 64
//!     pub x: f32,
//!     #[wire(i16, scale = 64.0)]
//!     pub z: f32,
//!     #[wire(i16, scale = 10000.0)] // [-pi, pi) fits i16 at 1e4
//!     pub heading: f32,
//!     #[wire(u8, scale = 32.0)]
//!     pub hp: f32,
//! }
//! ```
//!
//! Semantics: `wire = (value * scale).round() as <int>` — Rust float→int
//! casts saturate, so out-of-range values clamp to the int's limits (and
//! NaN becomes 0) instead of wrapping. Representable range is
//! `int::MIN/scale ..= int::MAX/scale`, resolution `1/scale`. The server
//! keeps full precision locally; only the replicated copy is quantized —
//! so don't quantize a predicted pool below its reconcile threshold, or
//! corrections never settle.

use proc_macro::TokenStream;
use proc_macro2::Literal;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{Data, DeriveInput, Fields, Ident, Lit, Token, parse_macro_input, spanned::Spanned};

/// `#[pm::pod]` — the one-line pod contract, and the POD COMPILER's
/// front door (engine-v2 item 1). Expands to `#[repr(C)]` plus the
/// standard derive set every replicated pod carries by convention
/// (`Clone, Copy, PartialEq, Debug, Default, Pod, Zeroable`), adds
/// `pm::Wire` automatically when any field has a `#[wire(..)]`
/// attribute, and — for named-field structs — GENERATES the pod's
/// blend semantics and schema identity:
///
/// - `pm::PodLerp` — fieldwise interpolation (the `interp_pool` lerp).
///   Tag angular f32 fields `#[lerp(angle)]` to lerp short-way and err
///   wrap-aware; integers and `Id`s are identity (never blend) by type.
/// - `pm::PodErr` — the fieldwise prediction-error metric (the
///   `predict_pool` err).
/// - `SCHEMA_HASH: u64` — an FNV over the pod's name and every field's
///   (name, type, wire quantization, lerp tag): two builds agree on it
///   iff they agree on everything that gives the bytes meaning. (The
///   versioned handshake it unlocks is the queued half of the item.)
///
/// ```ignore
/// #[pm::pod]
/// pub struct Hog {
///     #[wire(i16, scale = 64.0)]
///     pub x: f32,
///     #[wire(i16, scale = 10000.0)]
///     #[lerp(angle)]
///     pub heading: f32,
///     pub hp: f32, // pass-through on the wire, linear lerp
/// }
/// // pm.interp_pool(&hog, Hog::pod_lerp, …) — no hand lerp to forget.
/// ```
///
/// This must be an ATTRIBUTE macro, not part of `derive(Wire)`: a derive
/// only appends code next to its input — it can never inject `#[repr(C)]`
/// or more derives onto the struct itself. An attribute macro rewrites
/// the item, so the whole contract collapses into one line, for every
/// pod (quantized or not).
///
/// Don't repeat any of the standard derives below the attribute (that's
/// a conflicting-impl error); an existing `#[repr(..)]` is kept as-is.
/// The deriving crate needs `bytemuck` (and `pm`) as direct dependencies.
#[proc_macro_attribute]
pub fn pod(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[pm::pod] takes no arguments",
        )
        .to_compile_error()
        .into();
    }
    let input = parse_macro_input!(item as DeriveInput);
    expand_pod(input)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

fn expand_pod(mut input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let Data::Struct(data) = &mut input.data else {
        return Err(syn::Error::new(
            input.ident.span(),
            "#[pm::pod] only supports structs",
        ));
    };
    let name = input.ident.clone();
    let has_wire_fields = data
        .fields
        .iter()
        .any(|f| f.attrs.iter().any(|a| a.path().is_ident("wire")));
    let wire = has_wire_fields.then(|| quote! { , ::pm::Wire });

    // The generated blend/schema half (named-field structs only — a
    // unit/tuple pod keeps just the derive set).
    let mut compiled = proc_macro2::TokenStream::new();
    if let Fields::Named(fields) = &mut data.fields {
        let mut lerps = Vec::new();
        let mut errs = Vec::new();
        let mut schema = format!("{name}");
        for f in &mut fields.named {
            let fname = f.ident.clone().unwrap();
            let fty = f.ty.clone();
            // Parse AND STRIP #[lerp(..)] — it's this macro's attribute,
            // nothing downstream should see it. `angle` = cyclic f32
            // (short-way lerp, wrap-aware err); `snap` = identity (an id
            // or enum riding a float — never blend, newest wins).
            let mut angle = false;
            let mut snap = false;
            for a in f.attrs.iter().filter(|a| a.path().is_ident("lerp")) {
                let tag: Ident = a.parse_args()?;
                match () {
                    _ if tag == "angle" => angle = true,
                    _ if tag == "snap" => snap = true,
                    _ => {
                        return Err(syn::Error::new(
                            tag.span(),
                            "expected #[lerp(angle)] or #[lerp(snap)]",
                        ));
                    }
                }
                if !is_f32(&fty) {
                    return Err(syn::Error::new(
                        fty.span(),
                        "#[lerp(..)] tags only apply to f32 fields (other types have their meaning by type)",
                    ));
                }
            }
            f.attrs.retain(|a| !a.path().is_ident("lerp"));
            let wire_desc = f
                .attrs
                .iter()
                .find(|a| a.path().is_ident("wire"))
                .map(|a| quote!(#a).to_string())
                .unwrap_or_default();
            schema.push_str(&format!(
                "|{fname}:{}:{wire_desc}{}{}",
                quote!(#fty),
                if angle { ":angle" } else { "" },
                if snap { ":snap" } else { "" }
            ));
            if angle {
                lerps.push(quote! { #fname: ::pm::lerp_angle(self.#fname, b.#fname, t) });
                errs.push(quote! { ::pm::wrap_angle(self.#fname - b.#fname).abs() });
            } else if snap {
                lerps.push(quote! { #fname: b.#fname });
                errs.push(quote! { if self.#fname == b.#fname { 0.0 } else { 1.0 } });
            } else {
                lerps.push(quote! { #fname: ::pm::PodLerp::pod_lerp(&self.#fname, &b.#fname, t) });
                errs.push(quote! { ::pm::PodErr::pod_err(&self.#fname, &b.#fname) });
            }
        }
        compiled = quote! {
            impl ::pm::PodLerp for #name {
                fn pod_lerp(&self, b: &Self, t: f32) -> Self {
                    Self { #(#lerps,)* }
                }
            }
            impl ::pm::PodErr for #name {
                fn pod_err(&self, b: &Self) -> f32 {
                    0.0 #(+ #errs)*
                }
            }
            impl #name {
                /// Generated by `#[pm::pod]`: the identity of everything
                /// that gives this pod's bytes meaning (name, fields,
                /// types, quantization, lerp tags). Two builds agree on
                /// it iff their views of this pod agree.
                pub const SCHEMA_HASH: u64 = ::pm::schema_hash_str(#schema);
            }
        };
    }

    let has_repr = input.attrs.iter().any(|a| a.path().is_ident("repr"));
    let repr = (!has_repr).then(|| quote! { #[repr(C)] });
    Ok(quote! {
        #[derive(
            Clone, Copy, PartialEq, Debug, Default,
            ::bytemuck::Pod, ::bytemuck::Zeroable #wire
        )]
        #repr
        #input

        #compiled
    })
}

/// Parsed `#[wire(i16)]` / `#[wire(i16, scale = 64.0)]`.
struct WireAttr {
    ty: Ident,
    scale: f32,
}

impl Parse for WireAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let ty: Ident = input.parse()?;
        let mut scale = 1.0f32;
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            let key: Ident = input.parse()?;
            if key != "scale" {
                return Err(syn::Error::new(key.span(), "expected `scale = <number>`"));
            }
            input.parse::<Token![=]>()?;
            scale = match input.parse::<Lit>()? {
                Lit::Float(f) => f.base10_parse()?,
                Lit::Int(i) => i.base10_parse()?,
                other => return Err(syn::Error::new(other.span(), "scale must be a number")),
            };
        }
        if !input.is_empty() {
            return Err(input.error("expected `#[wire(<int type>)]` or `#[wire(<int type>, scale = <number>)]`"));
        }
        if scale <= 0.0 || !scale.is_finite() {
            return Err(syn::Error::new(ty.span(), "scale must be a positive finite number"));
        }
        const TARGETS: [&str; 4] = ["i8", "i16", "u8", "u16"];
        if !TARGETS.iter().any(|t| ty == t) {
            return Err(syn::Error::new(
                ty.span(),
                "quantization target must be one of i8, i16, u8, u16",
            ));
        }
        Ok(WireAttr { ty, scale })
    }
}

/// Whether `ty` is literally the token `f32` — quantized fields must be.
fn is_f32(ty: &syn::Type) -> bool {
    matches!(ty, syn::Type::Path(p) if p.qself.is_none() && p.path.is_ident("f32"))
}

#[proc_macro_derive(Wire, attributes(wire))]
pub fn derive_wire(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(input).unwrap_or_else(|e| e.to_compile_error()).into()
}

fn expand(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new(
            input.generics.span(),
            "#[derive(Wire)] does not support generic structs",
        ));
    }
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new(name.span(), "#[derive(Wire)] only supports structs"));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new(name.span(), "#[derive(Wire)] requires named fields"));
    };

    let repr_name = format_ident!("{name}Wire");
    let vis = &input.vis;

    // Per field: the repr field decl, both conversion directions, and (for
    // pass-through fields) a Pod bound assertion — packed + all-Pod fields
    // is what makes the hand-emitted `unsafe impl Pod` below sound.
    let mut repr_fields = Vec::new();
    let mut to_repr = Vec::new();
    let mut from_repr = Vec::new();
    let mut pod_asserts = Vec::new();
    for f in &fields.named {
        let fname = f.ident.as_ref().unwrap();
        let fty = &f.ty;
        let attr = f.attrs.iter().find(|a| a.path().is_ident("wire"));
        match attr {
            Some(a) => {
                let WireAttr { ty, scale } = a.parse_args()?;
                if !is_f32(fty) {
                    return Err(syn::Error::new(
                        fty.span(),
                        "#[wire(..)] quantization only applies to f32 fields",
                    ));
                }
                let scale = Literal::f32_suffixed(scale);
                repr_fields.push(quote! { #fname: #ty });
                // Float→int `as` saturates (NaN → 0): out-of-range values
                // clamp to the int's limits instead of wrapping.
                to_repr.push(quote! { #fname: (self.#fname * #scale).round() as #ty });
                from_repr.push(quote! { #fname: repr.#fname as f32 / #scale });
            }
            None => {
                repr_fields.push(quote! { #fname: #fty });
                to_repr.push(quote! { #fname: self.#fname });
                from_repr.push(quote! { #fname: repr.#fname });
                pod_asserts.push(quote! { assert_pod::<#fty>(); });
            }
        }
    }

    Ok(quote! {
        /// Generated by `#[derive(pm::Wire)]` — the compact pod that rides
        /// the wire in place of the game struct. Not for game code.
        #[doc(hidden)]
        #[repr(C, packed)]
        #[derive(Clone, Copy)]
        #vis struct #repr_name {
            #(#repr_fields,)*
        }

        // Sound: repr(C, packed) leaves no padding, and every field is Pod
        // (quantized fields are primitive ints; pass-through fields are
        // asserted below).
        unsafe impl ::bytemuck::Zeroable for #repr_name {}
        unsafe impl ::bytemuck::Pod for #repr_name {}

        const _: () = {
            fn assert_pod<T: ::bytemuck::Pod>() {}
            #[allow(dead_code)]
            fn pass_through_fields_must_be_pod() {
                #(#pod_asserts)*
            }
        };

        impl ::pm::Wire for #name {
            type Repr = #repr_name;
            fn to_repr(&self) -> #repr_name {
                #repr_name { #(#to_repr,)* }
            }
            fn from_repr(repr: #repr_name) -> #name {
                #name { #(#from_repr,)* }
            }
        }
    })
}
