//! The runtime value model.
//!
//! `na` is a first-class value, not a raw NaN convention. Everything that
//! touches unavailable data — out-of-range history reads, non-finite market
//! data, arithmetic on `na` — flows through [`Value::Na`]. Table displacement
//! retains one runtime-only unavailable-value reason, so the
//! representation can change (e.g. to tagged ints) without semantic churn.

use serde::de::{self, Deserializer, Visitor};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU64;

/// One runtime value.
///
/// Size tradeoff: strings and color provenance make this an owned, tagged,
/// non-`Copy` value. The VM charges storage using `size_of::<Value>()`, so
/// provenance storage is included in its memory limits on each target.
/// Every history cell pays that footprint even for numeric-only slots. If
/// long-history memory or throughput becomes a constraint, the escape hatch
/// is numeric-specialized history with side tables for strings and colors.
#[derive(Debug, Clone, Default)]
#[cfg_attr(
    all(charon, feature = "aeneas-color-clone"),
    charon::rename("CloneColorValue")
)]
pub enum Value {
    /// Unavailable/undefined, Pine's `na`.
    #[default]
    Na,
    /// An unavailable table alias after same-position table replacement.
    /// Ordinary observations are `na`; the runtime retains the reason so
    /// `table.cell` can distinguish displacement from an unassigned table.
    /// The run-local lifetime identity survives retirement and rollback,
    /// without retaining a heap reference. Decoding never synthesizes it.
    DisplacedTable(
        /// Opaque identity of the original table lifetime, not a heap index.
        NonZeroU64,
    ),
    /// Boolean scalar.
    Bool(
        /// Boolean scalar value.
        bool,
    ),
    /// 64-bit signed integer with exact semantics. Integer overflow and
    /// division/remainder by zero yield [`Value::Na`] (checked arithmetic in
    /// the VM), never a wrapped or non-finite value.
    Int(
        /// Exact signed-integer scalar value.
        i64,
    ),
    /// 64-bit floating-point scalar. Compiler and VM producers keep it finite;
    /// direct constructors must uphold that invariant or use [`Value::from_f64`].
    Float(
        /// Floating-point scalar. Direct constructors must supply a finite
        /// value; use [`Value::from_f64`] to map non-finite inputs to `Na`.
        f64,
    ),
    /// UTF-8 string scalar.
    String(
        /// Owned UTF-8 scalar value.
        String,
    ),
    /// A color value. `r`/`g`/`b` are 8-bit channels; `t` is transparency in
    /// `0..=100` (0 = opaque, 100 = fully transparent), the same scale as the
    /// transparency argument of `color.new`. Stored decomposed rather than as a
    /// packed `u32` so fractional transparency survives computation and
    /// accessors. The wire hex string quantizes alpha to its 8-bit channel;
    /// parsing any literal alpha byte and serializing it preserves that byte.
    Color {
        /// Red channel in `0..=255`.
        r: u8,
        /// Green channel in `0..=255`.
        g: u8,
        /// Blue channel in `0..=255`.
        b: u8,
        /// Transparency percentage in `0..=100`.
        t: f64,
        /// Selected raw input.color identity. Internal provenance only; it does
        /// not participate in Pine equality or the color hex wire value.
        input_origin: Option<u32>,
    },
    /// A handle to a heap-allocated collection (array/matrix/map). The handle
    /// is an index into the per-run VM heap (see [`crate::heap`]); it is *not*
    /// the collection's contents. Assignment copies the handle — two variables
    /// holding the same `Ref` alias one heap object (Pine Script reference
    /// semantics). A `Ref` is never produced by the scalar arithmetic opcodes
    /// and never crosses the wasm wire boundary as data (a bare handle is
    /// meaningless outside its run); serialization treats it as `na`.
    Ref(
        /// Handle to the run-local collection.
        HandleId,
    ),
}

// Color provenance identifies a palette entry, not a different Pine
// value. Displaced tables retain identity for reference containers; ordinary
// Pine scalar comparisons separately normalize unavailable values to Na.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Na, Self::Na) => true,
            (Self::DisplacedTable(a), Self::DisplacedTable(b)) => a == b,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a == b,
            (Self::String(a), Self::String(b)) => a == b,
            (Self::Ref(a), Self::Ref(b)) => a == b,
            (
                Self::Color {
                    r: ar,
                    g: ag,
                    b: ab,
                    t: at,
                    ..
                },
                Self::Color {
                    r: br,
                    g: bg,
                    b: bb,
                    t: bt,
                    ..
                },
            ) => ar == br && ag == bg && ab == bb && at == bt,
            _ => false,
        }
    }
}

/// Index into the per-run VM heap. Handles are dense and run-local: they are
/// allocated in order from 0 and are only meaningful within the run that
/// produced them. See [`crate::heap::Heap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HandleId(
    /// Zero-based index into the heap owned by the run that produced it.
    pub u32,
);

impl HandleId {
    /// Convert the dense handle id to an index for the owning run's heap.
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// The handle a [`Value::Ref`] holds, else `None`: [`Value::as_ref`] as a free
/// function.
///
/// With [`set_ref`], this is the only way the heap rollback code
/// ([`crate::heap::restore_graph`]) looks inside a `Value`. The Aeneas-
/// extractable subset treats `Value` as opaque (its `Float(f64)` and `String`
/// payloads are not translated), so these two functions are handled separately.
pub fn ref_handle(value: &Value) -> Option<HandleId> {
    value.as_ref()
}

/// Overwrite `value` with the reference `Value::Ref(handle)`.
pub fn set_ref(value: &mut Value, handle: HandleId) {
    *value = Value::Ref(handle);
}

impl Value {
    /// Whether this value represents Pine's unavailable `na` value.
    pub fn is_na(&self) -> bool {
        matches!(self, Value::Na | Value::DisplacedTable(_))
    }

    /// Observe an unavailable table alias as ordinary `na` for scalar kernels.
    /// Stores and copies must retain the original value, so a later table
    /// operation still has its receiver provenance.
    #[inline]
    pub fn na_observation(&self) -> &Self {
        match self {
            Value::DisplacedTable(_) => &Value::Na,
            _ => self,
        }
    }

    /// Owned counterpart of [`Value::na_observation`] for scalar consumers.
    #[inline]
    pub fn into_na_observation(self) -> Self {
        match self {
            Value::DisplacedTable(_) => Value::Na,
            value => value,
        }
    }

    /// The handle if this value is a collection reference, else `None`.
    pub fn as_ref(&self) -> Option<HandleId> {
        match self {
            Value::Ref(h) => Some(*h),
            _ => None,
        }
    }

    /// Format a color: fractional transparency is normalized to four decimal
    /// places, then rounded to the nearest alpha byte. Percentage fractions
    /// survive until this paint boundary.
    pub fn color_hex(r: u8, g: u8, b: u8, t: f64) -> String {
        let t = t.clamp(0.0, 100.0);
        let alpha = if t.fract() == 0.0 {
            color_alpha(t as u8)
        } else {
            // Normalize alpha to four decimals before converting the
            // normalized RGBA value to an integer.
            let normalized = ((1.0 - t / 100.0) * 10_000.0).round() / 10_000.0;
            Some((255.0 * normalized).round() as u8)
        };
        match alpha {
            None => format!("#{r:02X}{g:02X}{b:02X}"),
            Some(alpha) => format!("#{r:02X}{g:02X}{b:02X}{alpha:02X}"),
        }
    }

    /// Wrap an `f64`, mapping non-finite values to `Na` so NaN never leaks
    /// into the value domain.
    pub fn from_f64(v: f64) -> Value {
        if v.is_finite() {
            Value::Float(v)
        } else {
            Value::Na
        }
    }

    /// Runtime type label used in structured type-mismatch diagnostics.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Na | Value::DisplacedTable(_) => "na",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::String(_) => "string",
            Value::Color { .. } => "color",
            // A bare handle has no element type without the heap; the generic
            // label is only used in defense-in-depth diagnostics.
            Value::Ref(_) => "collection",
        }
    }

    /// Compact display form used in IR dumps and test output.
    pub fn render(&self) -> String {
        match self {
            Value::Na | Value::DisplacedTable(_) => "na".into(),
            Value::Bool(b) => b.to_string(),
            Value::Int(v) => v.to_string(),
            Value::Float(v) => v.to_string(),
            Value::String(s) => format!("{s:?}"),
            Value::Color { r, g, b, t, .. } => Value::color_hex(*r, *g, *b, *t),
            Value::Ref(h) => format!("ref#{}", h.0),
        }
    }
}

/// Integer core of [`Value::color_hex`]: the alpha byte for transparency `t`,
/// or `None` when the color is fully opaque (`t == 0`) and no alpha byte is
/// emitted. `t` saturates at 100, and the alpha channel is
/// `(255 * (100 - t) + 50) / 100` in `u32`, rounded to nearest.
///
/// Kept free of `String`/formatting so it stays inside the Aeneas-extractable
/// subset (see the crate's `aeneas` feature).
pub(crate) fn color_alpha(t: u8) -> Option<u8> {
    let t = t.min(100);
    if t == 0 {
        None
    } else {
        Some(((255u32 * (100 - t) as u32 + 50) / 100) as u8)
    }
}

/// JSON mapping: `Na` ⇔ `null`, `Bool` ⇔ boolean, `Int` → number,
/// `Float` ⇔ number, `String` ⇔ string. This is the persistence mapping
/// used by `serde_json`. The direct WebAssembly adapter deliberately configures
/// `serde_wasm_bindgen` to emit the same `Int` as a JavaScript `bigint` instead.
///
/// The int mapping is deliberately **asymmetric**: `Int` serializes as a
/// plain JSON number, but deserialization of whole JSON numbers keeps
/// mapping to `Float` (as it always has). The wire format is simply
/// "numeric" — `Int` exists for exact runtime semantics inside the VM, not
/// as a distinct wire type, so `Int(3)` round-trips through JSON as
/// `Float(3.0)`.
impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::Na | Value::DisplacedTable(_) => serializer.serialize_none(),
            Value::Bool(b) => serializer.serialize_bool(*b),
            Value::Int(v) => serializer.serialize_i64(*v),
            Value::Float(v) => serializer.serialize_f64(*v),
            Value::String(s) => serializer.serialize_str(s),
            // Colors serialize as their `#RRGGBB[AA]` hex string. Like `Int`,
            // this is a one-way wire mapping: a JSON string deserializes back
            // to `Value::String`, never to `Value::Color` (colors exist for
            // runtime semantics inside the VM, not as a distinct wire type).
            Value::Color { r, g, b, t, .. } => {
                serializer.serialize_str(&Value::color_hex(*r, *g, *b, *t))
            }
            // A raw heap handle is meaningless outside its run and never
            // appears as a plot value (plots are scalar), so it serializes as
            // `null` (na) — the same one-way policy as `Int`/`Color`. Hosts
            // that need collection contents read them through a dedicated
            // accessor, not the plot wire format.
            Value::Ref(_) => serializer.serialize_none(),
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ValueVisitor;

        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = Value;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("null, a boolean, a number, or a string")
            }

            fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
                Ok(Value::Na)
            }

            fn visit_none<E: de::Error>(self) -> Result<Value, E> {
                Ok(Value::Na)
            }

            fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
                Value::deserialize(d)
            }

            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
                Ok(Value::Bool(v))
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
                Ok(Value::from_f64(v))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
                Ok(Value::Float(v as f64))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
                Ok(Value::Float(v as f64))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
                Ok(Value::String(v.to_string()))
            }

            fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
                Ok(Value::String(v))
            }
        }

        deserializer.deserialize_any(ValueVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displaced_table_keeps_na_observations_but_not_a_heap_root_or_wire_token() {
        let displaced = Value::DisplacedTable(NonZeroU64::new(7).unwrap());
        assert!(displaced.is_na());
        assert_eq!(displaced.type_name(), Value::Na.type_name());
        assert_eq!(displaced.render(), Value::Na.render());
        assert_ne!(displaced, Value::Na);
        assert_eq!(
            displaced,
            Value::DisplacedTable(NonZeroU64::new(7).unwrap())
        );
        assert_ne!(
            displaced,
            Value::DisplacedTable(NonZeroU64::new(8).unwrap())
        );
        assert_eq!(displaced.as_ref(), None);
        assert_eq!(ref_handle(&displaced), None);
        assert!(matches!(displaced.clone(), Value::DisplacedTable(_)));
        assert!(matches!(displaced.na_observation(), Value::Na));
        assert!(matches!(displaced.into_na_observation(), Value::Na));
        let wire =
            serde_json::to_string(&Value::DisplacedTable(NonZeroU64::new(7).unwrap())).unwrap();
        assert_eq!(wire, "null");
        assert!(matches!(
            serde_json::from_str::<Value>(&wire).unwrap(),
            Value::Na
        ));
        assert!(serde_json::from_str::<Value>(r#"{"DisplacedTable":null}"#).is_err());
    }

    #[test]
    fn json_round_trip() {
        let values = vec![
            Value::Na,
            Value::Bool(true),
            Value::Float(2.5),
            Value::String("hi".into()),
        ];
        let json = serde_json::to_string(&values).unwrap();
        assert_eq!(json, r#"[null,true,2.5,"hi"]"#);
        let back: Vec<Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, values);
    }

    #[test]
    fn int_serializes_as_number_but_deserializes_as_float() {
        // Documented asymmetry: the wire format is numeric; Int exists for
        // exact runtime semantics only.
        assert_eq!(serde_json::to_string(&Value::Int(3)).unwrap(), "3");
        assert_eq!(
            serde_json::to_string(&Value::Int(i64::MAX)).unwrap(),
            i64::MAX.to_string()
        );
        let back: Value = serde_json::from_str("3").unwrap();
        assert_eq!(back, Value::Float(3.0));
    }

    #[test]
    fn non_finite_floats_become_na() {
        assert_eq!(Value::from_f64(f64::NAN), Value::Na);
        assert_eq!(Value::from_f64(f64::INFINITY), Value::Na);
        assert_eq!(Value::from_f64(1.5), Value::Float(1.5));
    }

    #[test]
    fn color_hex_opaque_omits_alpha() {
        // t = 0 (fully opaque) → six-digit #RRGGBB, no alpha byte.
        assert_eq!(Value::color_hex(0xFF, 0x00, 0x00, 0.0), "#FF0000");
        assert_eq!(Value::color_hex(0x33, 0x66, 0x99, 0.0), "#336699");
        assert_eq!(Value::color_hex(0, 0, 0, 0.0), "#000000");
    }

    #[test]
    fn color_hex_translucent_appends_alpha() {
        // t = 100 (fully transparent) → alpha 00.
        assert_eq!(Value::color_hex(0x12, 0x34, 0x56, 100.0), "#12345600");
        // t = 50 → nearest alpha byte 128 = 0x80.
        assert_eq!(Value::color_hex(0x00, 0xFF, 0x00, 50.0), "#00FF0080");
        // t = 25 → nearest alpha byte 191 = 0xBF.
        assert_eq!(Value::color_hex(0xAB, 0xCD, 0xEF, 25.0), "#ABCDEFBF");
        // t = 2 → nearest alpha byte 250 = 0xFA.
        assert_eq!(Value::color_hex(0x00, 0x00, 0x00, 2.0), "#000000FA");
    }

    #[test]
    fn color_serializes_as_hex_string() {
        assert_eq!(
            serde_json::to_string(&Value::Color {
                r: 0xFF,
                g: 0x00,
                b: 0x00,
                t: 0.0,
                input_origin: None
            })
            .unwrap(),
            "\"#FF0000\""
        );
        assert_eq!(
            serde_json::to_string(&Value::Color {
                r: 0,
                g: 0,
                b: 0xFF,
                t: 50.0,
                input_origin: None
            })
            .unwrap(),
            "\"#0000FF80\""
        );
    }

    #[test]
    fn ref_handle_copy_aliases_same_object() {
        // A `Value::Ref` is a plain handle: cloning the value copies the
        // handle (aliasing), not the collection. This is the value-level half
        // of Pine Script reference assignment.
        let r = Value::Ref(HandleId(7));
        let copy = r.clone();
        assert_eq!(r, copy);
        assert_eq!(r.as_ref(), Some(HandleId(7)));
        assert_eq!(copy.as_ref(), Some(HandleId(7)));
        assert_eq!(Value::Float(1.0).as_ref(), None);
    }

    #[test]
    fn ref_serializes_as_null_and_renders_as_handle() {
        // Raw handles never cross the wire as data (meaningless outside the
        // run); they serialize to null like Int/Color's one-way mapping.
        assert_eq!(
            serde_json::to_string(&Value::Ref(HandleId(3))).unwrap(),
            "null"
        );
        assert_eq!(Value::Ref(HandleId(3)).render(), "ref#3");
        assert_eq!(Value::Ref(HandleId(0)).type_name(), "collection");
    }

    #[test]
    fn color_input_origin_survives_clone_without_changing_equality_or_wire() {
        let a = Value::Color {
            r: 255,
            g: 0,
            b: 0,
            t: 0.0,
            input_origin: Some(0),
        };
        let b = Value::Color {
            r: 255,
            g: 0,
            b: 0,
            t: 0.0,
            input_origin: Some(1),
        };
        assert_eq!(a, b);
        assert_eq!(a.render(), b.render());
        assert_eq!(serde_json::to_string(&a).unwrap(), "\"#FF0000\"");
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
        assert!(matches!(
            b.clone(),
            Value::Color {
                input_origin: Some(1),
                ..
            }
        ));
        let different = Value::Color {
            r: 0,
            g: 0,
            b: 255,
            t: 0.0,
            input_origin: Some(1),
        };
        assert_ne!(a, different);
    }

    #[test]
    fn color_type_name_and_render() {
        let c = Value::Color {
            r: 1,
            g: 2,
            b: 3,
            t: 0.0,
            input_origin: None,
        };
        assert_eq!(c.type_name(), "color");
        assert_eq!(c.render(), "#010203");
    }

    /// Compare all whole percentages with the expected alpha-byte table, and
    /// check the u8 saturation boundary explicitly.
    #[test]
    fn color_hex_matches_frozen_alpha_and_rounds_correctly() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/rendering/frozen-color-alpha.json"
        ))
        .unwrap();
        let samples = fixture["wholePercentages"].as_array().unwrap();
        assert_eq!(samples.len(), 101);
        let mut changed = 0;
        for t in 0..=u8::MAX {
            let expected = &samples[usize::from(t.min(100))];
            let expected_alpha = expected["alphaByte"].as_u64().unwrap() as u8;
            assert_eq!(
                Value::color_hex(18, 52, 86, f64::from(t)),
                expected["hex"].as_str().unwrap()
            );
            assert_eq!(color_alpha(t), (t != 0).then_some(expected_alpha));
            if t <= 100 {
                let old_alpha = (255u32 * u32::from(100 - t) / 100) as u8;
                changed += usize::from(old_alpha != expected_alpha);
            }
        }
        assert_eq!(
            changed, 50,
            "truncating the alpha byte differs from the table for half the whole percentages"
        );
    }
}
