//! The wire contract of the declared-lane schema: `SlotDef::lane`,
//! `ObjectTypeDecl::field_lanes` and [`Lane`]. Consumers of the JSON depend
//! on these exact forms, so every form is pinned here as literal text.

use crate::{CsilProgram, Lane, ObjectTypeDecl, SlotDef};

/// A real compiled program with one user slot (`x`) and no declarations.
fn compiled() -> CsilProgram {
    let parsed = pinewasm_parse::parse("//@version=6\nindicator(\"Lanes\")\nx = close\nplot(x)\n");
    let analyzed = pinewasm_semantics::analyze(&parsed.program);
    crate::lower_mir(&pinewasm_mir::lower(&analyzed.hir)).expect("real lowering")
}

fn slot(lane: Option<Lane>) -> SlotDef {
    SlotDef {
        name: "x".into(),
        persistent: false,
        varip: false,
        lane,
    }
}

fn object(field_lanes: Vec<Option<Lane>>) -> ObjectTypeDecl {
    ObjectTypeDecl {
        name: "T".into(),
        field_count: 2,
        field_lanes,
    }
}

fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("serializes")
}

#[test]
fn every_lane_variant_has_its_pinned_json_form() {
    let forms = [
        (Lane::F64, "\"F64\""),
        (Lane::I64, "\"I64\""),
        (Lane::Bool, "\"Bool\""),
        (Lane::Color, "\"Color\""),
        (Lane::Str, "\"Str\""),
        (Lane::Ref, "\"Ref\""),
        (Lane::Obj(0), "{\"Obj\":0}"),
        (Lane::Obj(9), "{\"Obj\":9}"),
        (Lane::Obj(u32::MAX), "{\"Obj\":4294967295}"),
    ];
    for (lane, text) in forms {
        assert_eq!(json(&lane), text);
        assert_eq!(serde_json::from_str::<Lane>(text).expect(text), lane);
    }
    // Nothing else is a lane: no lowercase spelling, no bare `Obj`, no
    // payload on a unit variant, no second key, no out-of-range type id.
    for text in [
        "\"f64\"",
        "\"Int\"",
        "\"Obj\"",
        "{\"I64\":0}",
        "{\"Obj\":0,\"Ref\":0}",
        "{\"Obj\":-1}",
        "{\"Obj\":4294967296}",
        "{\"Obj\":\"0\"}",
        "null",
    ] {
        assert!(serde_json::from_str::<Lane>(text).is_err(), "{text}");
    }
}

#[test]
fn a_slot_lane_is_absent_when_none_and_reads_null_as_none() {
    // `None` writes no key, so the slot keeps the three-key form.
    assert_eq!(
        json(&slot(None)),
        r#"{"name":"x","persistent":false,"varip":false}"#
    );
    assert_eq!(
        json(&slot(Some(Lane::I64))),
        r#"{"name":"x","persistent":false,"varip":false,"lane":"I64"}"#
    );
    assert_eq!(
        json(&slot(Some(Lane::Obj(9)))),
        r#"{"name":"x","persistent":false,"varip":false,"lane":{"Obj":9}}"#
    );
    // An absent key and an explicit null both read as `None`.
    for text in [
        r#"{"name":"x","persistent":false,"varip":false}"#,
        r#"{"name":"x","persistent":false,"varip":false,"lane":null}"#,
    ] {
        assert_eq!(
            serde_json::from_str::<SlotDef>(text).expect(text),
            slot(None)
        );
    }
    assert_eq!(
        serde_json::from_str::<SlotDef>(
            r#"{"name":"x","persistent":false,"varip":false,"lane":{"Obj":9}}"#
        )
        .expect("declared slot"),
        slot(Some(Lane::Obj(9)))
    );
    assert!(serde_json::from_str::<SlotDef>(
        r#"{"name":"x","persistent":false,"varip":false,"lane":"Float"}"#
    )
    .is_err());
}

#[test]
fn field_lanes_are_absent_when_empty_and_keep_null_entries() {
    // Empty writes no key, so the object keeps the two-key form.
    assert_eq!(json(&object(Vec::new())), r#"{"name":"T","field_count":2}"#);
    assert_eq!(
        json(&object(vec![Some(Lane::I64), None])),
        r#"{"name":"T","field_count":2,"field_lanes":["I64",null]}"#
    );
    assert_eq!(
        json(&object(vec![Some(Lane::Obj(3)), Some(Lane::Str)])),
        r#"{"name":"T","field_count":2,"field_lanes":[{"Obj":3},"Str"]}"#
    );
    // An absent key and an explicit `[]` both read as "no claim".
    for text in [
        r#"{"name":"T","field_count":2}"#,
        r#"{"name":"T","field_count":2,"field_lanes":[]}"#,
    ] {
        assert_eq!(
            serde_json::from_str::<ObjectTypeDecl>(text).expect(text),
            object(Vec::new())
        );
    }
    // A null entry is "no claim for that field" and survives a round trip.
    let text = r#"{"name":"T","field_count":2,"field_lanes":[null,"F64"]}"#;
    let decoded = serde_json::from_str::<ObjectTypeDecl>(text).expect(text);
    assert_eq!(decoded, object(vec![None, Some(Lane::F64)]));
    assert_eq!(json(&decoded), text);
    // An all-null vector is not empty, so it is written.
    assert_eq!(
        json(&object(vec![None, None])),
        r#"{"name":"T","field_count":2,"field_lanes":[null,null]}"#
    );
    // A null vector is not a vector.
    assert!(serde_json::from_str::<ObjectTypeDecl>(
        r#"{"name":"T","field_count":2,"field_lanes":null}"#
    )
    .is_err());
    // Serde does not enforce the cardinality rule (empty, or exactly
    // `field_count` entries); that is the lane checker's first obligation.
    assert_eq!(
        serde_json::from_str::<ObjectTypeDecl>(
            r#"{"name":"T","field_count":2,"field_lanes":["I64"]}"#
        )
        .expect("decodes")
        .field_lanes
        .len(),
        1
    );
}

#[test]
fn a_compiled_program_carries_no_declaration_and_writes_neither_key() {
    let program = compiled();
    assert!(program.series_slots.iter().all(|slot| slot.lane.is_none()));
    assert!(program
        .object_types
        .iter()
        .all(|decl| decl.field_lanes.is_empty()));
    let text = program.to_json();
    assert!(!text.contains("\"lane\"") && !text.contains("\"field_lanes\""));
    assert_eq!(CsilProgram::from_json(&text).expect("round trip"), program);
}

#[test]
fn a_declared_program_round_trips_and_differs_only_by_the_two_keys() {
    let plain = compiled();
    let plain_text = plain.to_json();
    let mut program = plain.clone();
    let slot = program
        .series_slots
        .iter()
        .position(|slot| slot.name == "x")
        .expect("slot x");
    program.series_slots[slot].lane = Some(Lane::F64);
    assert_eq!(program.object_types[0].name, "chart.point");
    program.object_types[0].field_lanes = vec![Some(Lane::F64), None, Some(Lane::Obj(0))];
    let declared = program.to_json();
    assert_eq!(
        declared,
        plain_text
            .replacen(
                r#"{"name":"x","persistent":false,"varip":false}"#,
                r#"{"name":"x","persistent":false,"varip":false,"lane":"F64"}"#,
                1
            )
            .replacen(
                r#"{"name":"chart.point","field_count":3}"#,
                r#"{"name":"chart.point","field_count":3,"field_lanes":["F64",null,{"Obj":0}]}"#,
                1
            )
    );
    assert_ne!(declared, plain_text);
    assert_eq!(
        CsilProgram::from_json(&declared).expect("round trip"),
        program
    );
}

/// The admitted-scalar projection (`validate/scalar_fields.rs`, built only
/// with the `aeneas` feature) describes a slot as `(name, persistent,
/// varip)`. It does not carry the lane, on purpose: it is not a serialization
/// or a full payload identity. Two programs that differ only by a declaration
/// therefore project equal, and anything built on the projection must not be
/// read as covering declarations.
#[cfg(feature = "aeneas")]
#[test]
fn the_scalar_projection_ignores_a_declared_lane() {
    use crate::validate::scalar_fields::observe_program;
    let plain = compiled();
    let mut declared = plain.clone();
    for slot in &mut declared.series_slots {
        slot.lane = Some(Lane::F64);
    }
    declared.object_types[0].field_lanes = vec![Some(Lane::F64), None, None];
    assert_ne!(plain, declared);
    assert_eq!(observe_program(&plain), observe_program(&declared));
    assert!(observe_program(&declared)
        .slots
        .contains(&(b"x".to_vec(), false, false)));
}
