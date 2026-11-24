//! The scalar `array.get` hot path. Errors and string clones stay with the
//! general collection handler, so its error ordering and string metering remain
//! authoritative.

use pinewasm_mir::heap::{Heap, HeapObj};
use pinewasm_mir::Value;

/// Check before narrowing to the target's pointer width. In particular, a
/// positive i64 whose low 32 bits name an element must not wrap on wasm32.
#[inline]
pub(crate) fn checked_index(index: i64, len: usize) -> Option<usize> {
    if index < 0 || index as u64 >= len as u64 {
        None
    } else {
        Some(index as usize)
    }
}

/// The element an `array.get`, `array.set` or `array.remove` index names. A
/// negative index counts from the end (`-1` is the last element); `-len` is
/// the first. Anything else out of range is `None`.
#[inline]
pub(crate) fn resolved_index(index: i64, len: usize) -> Option<usize> {
    if index < 0 {
        let from_end = index.unsigned_abs();
        if from_end > len as u64 {
            None
        } else {
            Some(len - from_end as usize)
        }
    } else {
        checked_index(index, len)
    }
}

/// An eligible successful read, with no heap mutation or string allocation.
/// `None` delegates to the general handler; it is not Pine's `na` value.
#[inline]
pub(crate) fn evaluate(heap: &Heap, args: &[Value]) -> Option<Value> {
    let [Value::Ref(handle), Value::Int(index)] = args else {
        return None;
    };
    let HeapObj::Array { items, .. } = heap.get(*handle)? else {
        return None;
    };
    let index = resolved_index(*index, items.len())?;
    let value = &items[index];
    // The general handler charges cloned scalar strings. References alias
    // their existing objects and do not clone any referenced string payload.
    if matches!(value, Value::String(_)) {
        return None;
    }
    Some(value.clone())
}

#[cfg(test)]
mod tests {
    use super::{checked_index, evaluate, resolved_index};
    use pinewasm_mir::heap::{ElemType, Heap, HeapObj};
    use pinewasm_mir::Value;

    #[test]
    fn index_boundaries_do_not_depend_on_pointer_width() {
        for (index, len, expected) in [
            (i64::MIN, 3, None),
            (-1, 3, None),
            (0, 0, None),
            (0, 3, Some(0)),
            (2, 3, Some(2)),
            (3, 3, None),
            (1_i64 << 32, 3, None),
            ((1_i64 << 32) + 1, 3, None),
            (i64::MAX, 3, None),
        ] {
            assert_eq!(checked_index(index, len), expected);
        }
    }

    #[test]
    fn negative_indices_count_from_the_end() {
        for (index, len, expected) in [
            (-1, 3, Some(2)),
            (-3, 3, Some(0)),
            (-4, 3, None),
            (-1, 0, None),
            (i64::MIN, 3, None),
            (0, 3, Some(0)),
            (3, 3, None),
            (1_i64 << 32, 3, None),
        ] {
            assert_eq!(resolved_index(index, len), expected, "{index} in {len}");
        }
    }

    #[test]
    fn scalar_reads_preserve_values_and_delegate_strings_and_bad_indices() {
        let mut heap = Heap::new();
        let values = vec![
            Value::Na,
            Value::Bool(true),
            Value::Int(i64::MIN),
            Value::Float(-0.0),
            Value::Color {
                r: 1,
                g: 2,
                b: 3,
                t: 4.0,
                input_origin: Some(7),
            },
            Value::String("meter me".into()),
        ];
        let handle = heap
            .alloc(HeapObj::Array {
                elem_ty: ElemType::Float,
                items: values.clone(),
            })
            .unwrap();
        for (index, value) in values.iter().enumerate().take(5) {
            let result = evaluate(&heap, &[Value::Ref(handle), Value::Int(index as i64)]);
            assert_eq!(result.as_ref(), Some(value));
            if let Some(Value::Color { input_origin, .. }) = result.as_ref() {
                assert_eq!(*input_origin, Some(7));
            }
            if let Some(Value::Float(number)) = result {
                assert_eq!(number.to_bits(), (-0.0_f64).to_bits());
            }
        }
        for index in [-1, 5, 6, 1_i64 << 32, i64::MAX] {
            assert_eq!(
                evaluate(&heap, &[Value::Ref(handle), Value::Int(index)]),
                None
            );
        }
        assert_eq!(evaluate(&heap, &[Value::Ref(handle), Value::Na]), None);
        assert_eq!(evaluate(&heap, &[Value::Na, Value::Int(0)]), None);
        let object = heap
            .alloc(HeapObj::Object {
                table_identity: None,
                type_id: 0,
                fields: vec![],
            })
            .unwrap();
        assert_eq!(evaluate(&heap, &[Value::Ref(object), Value::Int(0)]), None);
        let refs = heap
            .alloc(HeapObj::Array {
                elem_ty: ElemType::Object(0),
                items: vec![Value::Ref(object)],
            })
            .unwrap();
        assert_eq!(
            evaluate(&heap, &[Value::Ref(refs), Value::Int(0)]),
            Some(Value::Ref(object))
        );
    }
}
