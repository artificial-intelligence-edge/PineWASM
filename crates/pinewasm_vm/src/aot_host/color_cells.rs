//! Chart colors that cannot fit a packed RGBT cell retain their full value
//! in a run-local table. The low byte 254 is outside valid transparency
//! (0..=100) and distinct from missing (-1); the upper bits are an index.
//! Keeping the cell i32-sized lets native slots, history and heap arrays
//! carry colors without changing their storage or copying a full Value.
//!
//! A script that derives a color on every bar (a gradient, a transparency
//! that follows a series) adds entries on every bar, and its color lanes
//! refer to them until the run ends, so the table is what a long chart
//! grows by. An entry is therefore 24 bytes and nothing else: a color that
//! recurs is found through a small fixed cache of recent entries rather
//! than an index of all of them. Two handles may then name equal colors,
//! which is why a module compares full colors through the host.

use pinewasm_mir::Value;

const HANDLE_MARKER: u32 = 254;
/// The most entries a handle can name while its cell stays a non-negative
/// `i32`. Memory runs out long before: the cap only keeps the index honest.
const MAX_COLORS: usize = (1 << 23) - 1;
/// Recent entries remembered for reuse. A power of two.
const RECENT: usize = 1024;

/// One full color: what [`Value::Color`] holds.
#[derive(Clone, Copy, PartialEq)]
struct Full {
    rgb: [u8; 3],
    /// The transparency's bits, so `0.0` and `-0.0` stay distinct.
    transparency: u64,
    input_origin: Option<u32>,
}

impl Full {
    fn value(self) -> Value {
        Value::Color {
            r: self.rgb[0],
            g: self.rgb[1],
            b: self.rgb[2],
            t: f64::from_bits(self.transparency),
            input_origin: self.input_origin,
        }
    }

    /// Where this color is remembered in the cache of recent entries.
    fn recent_slot(self) -> usize {
        let origin = self.input_origin.map_or(0, |origin| u64::from(origin) + 1);
        let mixed = (self.transparency
            ^ u64::from_le_bytes([self.rgb[0], self.rgb[1], self.rgb[2], 0, 0, 0, 0, 0])
            ^ origin.rotate_left(32))
        .wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (mixed >> 32) as usize & (RECENT - 1)
    }
}

pub(super) struct ColorCells {
    values: Vec<Full>,
    /// The index of the last entry that hashed to each slot, plus one.
    recent: Box<[u32; RECENT]>,
}

impl Default for ColorCells {
    fn default() -> Self {
        ColorCells {
            values: Vec::new(),
            recent: Box::new([0; RECENT]),
        }
    }
}

impl ColorCells {
    pub(super) fn encode(&mut self, value: &Value) -> Result<i64, String> {
        let Value::Color {
            r,
            g,
            b,
            t,
            input_origin,
        } = value
        else {
            return if matches!(value, Value::Na) {
                Ok(-1)
            } else {
                Err("runtime value does not fit the static color cell".into())
            };
        };
        if !t.is_finite() || !(0.0..=100.0).contains(t) {
            return Err("invalid native color transparency".into());
        }
        if input_origin.is_none() && t.fract() == 0.0 && !(*t == 0.0 && t.is_sign_negative()) {
            return Ok(i64::from(
                ((u32::from(*r) << 24) | (u32::from(*g) << 16) | (u32::from(*b) << 8) | *t as u32)
                    as i32,
            ));
        }
        let full = Full {
            rgb: [*r, *g, *b],
            transparency: t.to_bits(),
            input_origin: *input_origin,
        };
        let slot = full.recent_slot();
        let remembered = self.recent[slot] as usize;
        let index = if remembered != 0 && self.values[remembered - 1] == full {
            remembered - 1
        } else {
            // This host-storage cap requests the exact VM rerun; it never
            // changes Pine's verdict.
            if self.values.len() >= MAX_COLORS {
                return Err("native color table capacity reached".into());
            }
            self.values.push(full);
            self.recent[slot] = self.values.len() as u32;
            self.values.len() - 1
        };
        Ok(i64::from(((index as u32) << 8 | HANDLE_MARKER) as i32))
    }

    pub(super) fn decode(&self, cell: i64) -> Result<Value, String> {
        let raw = cell as u32;
        if raw == u32::MAX {
            return Ok(Value::Na);
        }
        let transparency = raw & 255;
        if transparency == HANDLE_MARKER {
            return self
                .values
                .get((raw >> 8) as usize)
                .map(|full| full.value())
                .ok_or_else(|| "native color handle is outside its run table".into());
        }
        if transparency > 100 {
            return Err("invalid packed native color transparency".into());
        }
        Ok(Value::Color {
            r: (raw >> 24) as u8,
            g: (raw >> 16) as u8,
            b: (raw >> 8) as u8,
            t: f64::from(transparency),
            input_origin: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_colors_round_trip_and_keep_distinct_input_origins() {
        let mut table = ColorCells::default();
        for r in [0, 127, 128, 255] {
            for t in [-0.0, 0.0, 47.6, 50.01, 50.02, 100.0] {
                for input_origin in [None, Some(0), Some(u32::MAX)] {
                    let value = Value::Color {
                        r,
                        g: 254,
                        b: 255,
                        t,
                        input_origin,
                    };
                    let cell = table.encode(&value).unwrap();
                    assert_eq!(table.decode(cell).unwrap(), value);
                    assert_eq!(table.encode(&value).unwrap(), cell);
                    assert_ne!(cell, -1);
                    assert_eq!(
                        (cell as u32 & 255) == HANDLE_MARKER,
                        input_origin.is_some()
                            || t.fract() != 0.0
                            || (t == 0.0 && t.is_sign_negative())
                    );
                }
            }
        }
        let missing = table.encode(&Value::Na).unwrap();
        assert_eq!(table.decode(missing).unwrap(), Value::Na);
    }

    /// An entry is found again while it is among the recent ones, and a
    /// color that is not found is simply stored again: both handles decode
    /// to the value. The table grows by distinct colors, not by reuse.
    #[test]
    fn recurring_colors_reuse_their_entry_and_many_distinct_ones_stay_correct() {
        let color = |t: f64| Value::Color {
            r: 1,
            g: 2,
            b: 3,
            t,
            input_origin: Some(7),
        };
        let mut table = ColorCells::default();
        let recurring = table.encode(&color(12.5)).unwrap();
        for _ in 0..10_000 {
            assert_eq!(table.encode(&color(12.5)).unwrap(), recurring);
        }
        assert_eq!(table.values.len(), 1);

        // Far more distinct colors than the cache remembers.
        let distinct = 20 * RECENT;
        let cells: Vec<i64> = (0..distinct)
            .map(|i| table.encode(&color(50.0 + i as f64 / 1e6)).unwrap())
            .collect();
        assert_eq!(table.values.len(), 1 + distinct);
        for (i, cell) in cells.iter().enumerate() {
            assert!(*cell > 0, "a handle is a positive cell");
            assert_eq!(table.decode(*cell).unwrap(), color(50.0 + i as f64 / 1e6));
        }
        // Whether or not the first color is still remembered, every handle
        // it was given names it.
        let again = table.encode(&color(12.5)).unwrap();
        assert_eq!(table.decode(again).unwrap(), color(12.5));
        assert_eq!(table.decode(recurring).unwrap(), color(12.5));
        assert!(std::mem::size_of::<Full>() <= 24);
    }

    #[test]
    fn handles_are_local_to_one_run_and_invalid_cells_are_rejected() {
        let mut table = ColorCells::default();
        let value = Value::Color {
            r: 255,
            g: 0,
            b: 0,
            t: 47.6,
            input_origin: Some(2),
        };
        let cell = table.encode(&value).unwrap();
        assert!(ColorCells::default().decode(cell).is_err());
        assert!(table.decode(i64::from(0x12_34_56_65u32)).is_err());
        assert!(table.decode(i64::from(0x12_34_56_feu32)).is_err());
    }
}
