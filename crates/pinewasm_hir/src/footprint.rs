//! Order-flow footprint object model.
//!
//! `request.footprint(...)` yields a `footprint` **object** carrying the current
//! bar's volume-footprint data; `footprint.rows(fp)` yields an
//! `array<volume_row>`. Both `footprint` and `volume_row` are registered object
//! types (like the drawing object types); their scalar accessors are plain
//! field reads and reuse the UDT `GetField` machinery (an `na` receiver is a
//! structured runtime error, never a panic).
//!
//! Data sourcing stays a host concern: the VM materializes the object from an
//! **optional** per-run host table (`MarketData::footprint_data`), and every
//! accessor returns `na`/empty/`false` by default. No real order-flow feed is
//! involved, so the footprint surface can be exercised in tests without external data.
//!
//! Member signatures for the footprint namespace. See the public Pine Script®
//! manual for the documented names.

use crate::Type;

/// The registered type name of the footprint object.
pub const FOOTPRINT_TYPE: &str = "footprint";
/// The registered type name of one footprint volume row.
pub const VOLUME_ROW_TYPE: &str = "volume_row";

/// One accessor member of a footprint-family object type: the member name (the
/// function invoked as `<ns>.<name>(receiver)`), the field slot it reads, and
/// the field's declared [`Type`].
#[derive(Debug, Clone, Copy)]
pub struct FpField {
    pub name: &'static str,
    pub slot: u32,
    pub ty: FpFieldTy,
}

/// A const-friendly descriptor of a footprint field's type. `Rows` is the one
/// reference-typed variant (`array<volume_row>`) and cannot live in a `const`
/// [`Type`], so it is materialized on demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FpFieldTy {
    Float,
    Bool,
    /// `array<volume_row>` (the `footprint.rows` field).
    RowsArray,
}

impl FpFieldTy {
    /// The owned [`Type`] this descriptor denotes.
    pub fn to_type(self) -> Type {
        match self {
            FpFieldTy::Float => Type::Float,
            FpFieldTy::Bool => Type::Bool,
            FpFieldTy::RowsArray => {
                Type::Array(Box::new(Type::Object(VOLUME_ROW_TYPE.to_string())))
            }
        }
    }
}

/// `footprint.*` accessor members in field-slot order. The scalar order-flow
/// aggregates read `na` by default; `rows` reads an empty `array<volume_row>`.
pub const FOOTPRINT_FIELDS: &[FpField] = &[
    FpField {
        name: "buy_volume",
        slot: 0,
        ty: FpFieldTy::Float,
    },
    FpField {
        name: "sell_volume",
        slot: 1,
        ty: FpFieldTy::Float,
    },
    FpField {
        name: "total_volume",
        slot: 2,
        ty: FpFieldTy::Float,
    },
    FpField {
        name: "delta",
        slot: 3,
        ty: FpFieldTy::Float,
    },
    FpField {
        name: "poc",
        slot: 4,
        ty: FpFieldTy::Float,
    },
    FpField {
        name: "vah",
        slot: 5,
        ty: FpFieldTy::Float,
    },
    FpField {
        name: "val",
        slot: 6,
        ty: FpFieldTy::Float,
    },
    FpField {
        name: "rows",
        slot: 7,
        ty: FpFieldTy::RowsArray,
    },
];

/// `volume_row.*` accessor members in field-slot order. Scalars read `na`,
/// imbalance predicates read `false`, by default.
pub const VOLUME_ROW_FIELDS: &[FpField] = &[
    FpField {
        name: "buy_volume",
        slot: 0,
        ty: FpFieldTy::Float,
    },
    FpField {
        name: "sell_volume",
        slot: 1,
        ty: FpFieldTy::Float,
    },
    FpField {
        name: "total_volume",
        slot: 2,
        ty: FpFieldTy::Float,
    },
    FpField {
        name: "delta",
        slot: 3,
        ty: FpFieldTy::Float,
    },
    FpField {
        name: "up_price",
        slot: 4,
        ty: FpFieldTy::Float,
    },
    FpField {
        name: "down_price",
        slot: 5,
        ty: FpFieldTy::Float,
    },
    FpField {
        name: "has_buy_imbalance",
        slot: 6,
        ty: FpFieldTy::Bool,
    },
    FpField {
        name: "has_sell_imbalance",
        slot: 7,
        ty: FpFieldTy::Bool,
    },
];

/// The number of scalar (non-array) leading fields of a `footprint` object.
/// The VM materializes these from the host table before the trailing `rows`
/// array field.
pub const FOOTPRINT_SCALAR_COUNT: usize = 7;
/// The total field count of a `footprint` object (scalars + `rows`).
pub const FOOTPRINT_FIELD_COUNT: usize = FOOTPRINT_FIELDS.len();
/// The field count of a `volume_row` object.
pub const VOLUME_ROW_FIELD_COUNT: usize = VOLUME_ROW_FIELDS.len();

/// Look up a `footprint.*` accessor member by name.
pub fn footprint_field(name: &str) -> Option<&'static FpField> {
    FOOTPRINT_FIELDS.iter().find(|f| f.name == name)
}

/// Look up a `volume_row.*` accessor member by name.
pub fn volume_row_field(name: &str) -> Option<&'static FpField> {
    VOLUME_ROW_FIELDS.iter().find(|f| f.name == name)
}
