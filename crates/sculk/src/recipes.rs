//! Crafting: matches a 3x3 grid against the vanilla shaped and shapeless
//! recipes in `crate::vanilla_items`.
//!
//! The occupied cells are cropped to their bounding box, so a pattern matches
//! wherever it sits in the grid (the 2x2 inventory grid arrives in cells 0, 1,
//! 3 and 4). Only the bucket of recipes with the same width, height and
//! occupied-cell count is scanned for shaped recipes, and only the bucket
//! with the same ingredient count for shapeless ones. When both kinds match,
//! the recipe whose id sorts first wins, as within each bucket.

use crate::inventory::Slot;
use crate::vanilla_items::{
    Shaped, Shapeless, EMPTY_CELL, ING_ITEMS, ING_OFFSETS, SHAPED, SHAPED_BUCKETS, SHAPED_CELLS,
    SHAPELESS, SHAPELESS_BUCKETS, SHAPELESS_INGS,
};

/// Whether ingredient set `set` accepts `item`.
fn accepts(set: u16, item: u16) -> bool {
    let set = usize::from(set);
    let items = &ING_ITEMS[usize::from(ING_OFFSETS[set])..usize::from(ING_OFFSETS[set + 1])];
    items.binary_search(&item).is_ok()
}

/// Whether `recipe`'s cells match `shape` (`w` wide), optionally mirrored
/// left to right.
fn shaped_matches(recipe: &Shaped, shape: &[u16], w: usize, mirror: bool) -> bool {
    let cells = &SHAPED_CELLS[usize::from(recipe.cells)..][..shape.len()];
    shape.iter().enumerate().all(|(i, &item)| {
        let x = if mirror { w - 1 - i % w } else { i % w };
        let want = cells[i - i % w + x];
        if want == EMPTY_CELL {
            item == EMPTY_CELL
        } else {
            item != EMPTY_CELL && accepts(want, item)
        }
    })
}

/// Assigns every ingredient to a distinct unused item by backtracking.
/// `failed` memoizes item subsets (by `used` mask) known not to work; the
/// ingredient depth is implied by the mask's population count.
fn assign(ings: &[u16], items: &[u16], used: u16, failed: &mut [u64; 8]) -> bool {
    let Some((&first, rest)) = ings.split_first() else {
        return true;
    };
    let bit = |mask: u16| (usize::from(mask) >> 6, 1u64 << (mask & 63));
    let (word, flag) = bit(used);
    if failed[word] & flag != 0 {
        return false;
    }
    for (i, &item) in items.iter().enumerate() {
        let mask = 1u16 << i;
        if used & mask == 0 && accepts(first, item) && assign(rest, items, used | mask, failed) {
            return true;
        }
    }
    failed[word] |= flag;
    false
}

fn shapeless_matches(recipe: &Shapeless, items: &[u16]) -> bool {
    let ings = &SHAPELESS_INGS[usize::from(recipe.ings)..][..items.len()];
    assign(ings, items, 0, &mut [0; 8])
}

/// The result of crafting `grid` (3x3, row-major), or an empty slot.
pub fn crafted_item(grid: &[Slot; 9]) -> Slot {
    let mut items = [0u16; 9];
    let mut n = 0;
    let (mut x0, mut y0, mut x1, mut y1) = (3, 3, 0, 0);
    for (i, slot) in grid.iter().enumerate() {
        if slot.is_empty() {
            continue;
        }
        match u16::try_from(slot.item_id) {
            Ok(id) if id != EMPTY_CELL => items[n] = id,
            _ => return Slot::default(),
        }
        n += 1;
        let (x, y) = (i % 3, i / 3);
        (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
    }
    if n == 0 {
        return Slot::default();
    }
    let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
    let mut shape = [EMPTY_CELL; 9];
    for (i, slot) in grid.iter().enumerate() {
        if !slot.is_empty() {
            shape[(i / 3 - y0) * w + (i % 3 - x0)] = slot.item_id as u16;
        }
    }
    let shape = &shape[..w * h];

    let bucket = ((w - 1) * 3 + (h - 1)) * 9 + (n - 1);
    let shaped = SHAPED
        [usize::from(SHAPED_BUCKETS[bucket])..usize::from(SHAPED_BUCKETS[bucket + 1])]
        .iter()
        .find(|r| {
            shaped_matches(r, shape, w, false)
                || (!r.symmetric && shaped_matches(r, shape, w, true))
        })
        .map(|r| (r.rank, r.result, r.count));
    let shapeless = SHAPELESS
        [usize::from(SHAPELESS_BUCKETS[n - 1])..usize::from(SHAPELESS_BUCKETS[n])]
        .iter()
        .find(|r| shapeless_matches(r, &items[..n]))
        .map(|r| (r.rank, r.result, r.count));

    let best = match (shaped, shapeless) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    };
    best.map_or_else(Slot::default, |(_, result, count)| Slot {
        count: i16::from(count),
        item_id: i32::from(result),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vanilla_items::item;

    const E: i32 = 0;

    /// Builds a grid from item ids (0 = empty), row-major.
    fn grid(cells: [i32; 9]) -> [Slot; 9] {
        cells.map(|item_id| Slot {
            count: if item_id == E { 0 } else { 1 },
            item_id,
        })
    }

    fn craft(cells: [i32; 9]) -> (i32, i16) {
        let s = crafted_item(&grid(cells));
        (s.item_id, s.count)
    }

    /// Places a 2x2 pattern at each of the four 2x2 corners of the 3x3 grid.
    fn corners(p: [i32; 4]) -> [[i32; 9]; 4] {
        [0, 1, 3, 4].map(|o| {
            let mut g = [E; 9];
            for (k, &it) in p.iter().enumerate() {
                g[o + (k / 2) * 3 + k % 2] = it;
            }
            g
        })
    }

    #[test]
    fn planks_from_any_log() {
        let logs = [
            (item::OAK_LOG, item::OAK_PLANKS),
            (item::STRIPPED_OAK_WOOD, item::OAK_PLANKS),
            (item::SPRUCE_LOG, item::SPRUCE_PLANKS),
            (item::BIRCH_WOOD, item::BIRCH_PLANKS),
            (item::JUNGLE_LOG, item::JUNGLE_PLANKS),
            (item::ACACIA_LOG, item::ACACIA_PLANKS),
            (item::DARK_OAK_LOG, item::DARK_OAK_PLANKS),
            (item::MANGROVE_LOG, item::MANGROVE_PLANKS),
            (item::CHERRY_LOG, item::CHERRY_PLANKS),
            (item::PALE_OAK_LOG, item::PALE_OAK_PLANKS),
            (item::CRIMSON_STEM, item::CRIMSON_PLANKS),
            (item::STRIPPED_WARPED_HYPHAE, item::WARPED_PLANKS),
            (item::BAMBOO_BLOCK, item::BAMBOO_PLANKS),
        ];
        for (log, planks) in logs {
            for pos in 0..9 {
                let mut g = [E; 9];
                g[pos] = log;
                let expected = if log == item::BAMBOO_BLOCK { 2 } else { 4 };
                assert_eq!(craft(g), (planks, expected), "log {log} at {pos}");
            }
        }
    }

    #[test]
    fn sticks_and_crafting_table() {
        let (o, b) = (item::OAK_PLANKS, item::BIRCH_PLANKS);
        assert_eq!(craft([o, E, E, o, E, E, E, E, E]), (item::STICK, 4));
        assert_eq!(craft([E, E, E, E, E, o, E, E, b]), (item::STICK, 4));
        for g in corners([o, o, o, o]) {
            assert_eq!(craft(g), (item::CRAFTING_TABLE, 1));
        }
        for g in corners([o, b, item::SPRUCE_PLANKS, o]) {
            assert_eq!(craft(g), (item::CRAFTING_TABLE, 1));
        }
        // An extra item spoils the pattern.
        assert_eq!(craft([o, o, E, o, o, E, E, E, item::STICK]), (E, 0));
    }

    #[test]
    fn pickaxes() {
        let (p, s, c) = (item::OAK_PLANKS, item::STICK, item::COBBLESTONE);
        assert_eq!(
            craft([p, p, p, E, s, E, E, s, E]),
            (item::WOODEN_PICKAXE, 1)
        );
        let mixed = item::CRIMSON_PLANKS;
        assert_eq!(
            craft([p, mixed, p, E, s, E, E, s, E]),
            (item::WOODEN_PICKAXE, 1)
        );
        assert_eq!(craft([c, c, c, E, s, E, E, s, E]), (item::STONE_PICKAXE, 1));
        let d = item::COBBLED_DEEPSLATE;
        assert_eq!(craft([c, d, c, E, s, E, E, s, E]), (item::STONE_PICKAXE, 1));
        assert_eq!(craft([c, c, c, E, s, E, s, E, E]), (E, 0));
    }

    #[test]
    fn mirrored_axe() {
        let (p, s) = (item::OAK_PLANKS, item::STICK);
        assert_eq!(craft([p, p, E, p, s, E, E, s, E]), (item::WOODEN_AXE, 1));
        assert_eq!(craft([E, p, p, E, s, p, E, s, E]), (item::WOODEN_AXE, 1));
        assert_eq!(craft([p, p, E, s, p, E, s, E, E]), (item::WOODEN_AXE, 1));
    }

    #[test]
    fn shapeless_recipes() {
        let (iron, flint) = (item::IRON_INGOT, item::FLINT);
        assert_eq!(
            craft([iron, flint, E, E, E, E, E, E, E]),
            (item::FLINT_AND_STEEL, 1)
        );
        assert_eq!(
            craft([E, E, E, E, E, flint, E, iron, E]),
            (item::FLINT_AND_STEEL, 1)
        );
        assert_eq!(
            craft([E, E, E, E, item::BONE, E, E, E, E]),
            (item::BONE_MEAL, 3)
        );
        // Ingredient order and position do not matter.
        let book = [item::BOOK, item::INK_SAC, item::FEATHER];
        assert_eq!(
            craft([book[2], book[1], book[0], E, E, E, E, E, E]),
            (item::WRITABLE_BOOK, 1)
        );
    }

    #[test]
    fn non_recipes_are_empty() {
        assert_eq!(craft([E; 9]), (E, 0));
        assert_eq!(craft([item::DIRT, E, E, E, E, E, E, E, E]), (E, 0));
        assert_eq!(
            craft([item::DIRT, item::STICK, E, E, E, E, E, E, E]),
            (E, 0)
        );
        assert_eq!(craft([item::STICK; 9]), (E, 0));
        assert_eq!(craft([-3, E, E, E, E, E, E, E, E]), (E, 0));
        let mut g = grid([item::OAK_LOG, E, E, E, E, E, E, E, E]);
        g[0].count = 0;
        assert_eq!(crafted_item(&g), Slot::default());
    }
}
