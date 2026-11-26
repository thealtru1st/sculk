//! The player inventory, laid out like the vanilla player inventory window:
//!
//! | slots  | contents          |
//! |--------|-------------------|
//! | 0      | crafting result   |
//! | 1-4    | 2x2 crafting grid |
//! | 5-8    | armor             |
//! | 9-35   | main inventory    |
//! | 36-44  | hotbar            |
//! | 45     | offhand           |

use crate::items::{max_stack, AIR_ITEM};
use std::ops::Range;

pub const INVENTORY_SIZE: usize = 46;

const MAIN: Range<usize> = 9..36;
const HOTBAR: Range<usize> = 36..45;
const OFFHAND: usize = 45;

/// One inventory slot. A slot with `count <= 0` or `AIR_ITEM` is empty.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Slot {
    pub count: i16,
    pub item_id: i32,
}

impl Slot {
    pub fn is_empty(&self) -> bool {
        self.count <= 0 || self.item_id == AIR_ITEM
    }
}

#[derive(Clone)]
pub struct Inventory {
    pub slots: [Slot; INVENTORY_SIZE],
}

impl Default for Inventory {
    fn default() -> Self {
        Self {
            slots: [Slot::default(); INVENTORY_SIZE],
        }
    }
}

impl Inventory {
    /// Stores `count` of `item_id` in `slot`; an empty stack clears the slot.
    /// Out-of-range slots are ignored.
    pub fn set(&mut self, slot: usize, count: i16, item_id: i32) {
        if let Some(s) = self.slots.get_mut(slot) {
            let stack = Slot { count, item_id };
            *s = if stack.is_empty() {
                Slot::default()
            } else {
                stack
            };
        }
    }

    /// The stack in `slot`, or an empty slot when out of range.
    pub fn get(&self, slot: usize) -> Slot {
        self.slots.get(slot).copied().unwrap_or_default()
    }

    /// Adds items the way a pickup does and returns how many did not fit.
    ///
    /// Existing stacks of the item are topped up first (hotbar, then main
    /// inventory, then offhand), then empty slots are filled (hotbar, then
    /// main inventory), each stack respecting the item's max stack size.
    /// Crafting, armor and offhand slots never receive new stacks.
    pub fn insert(&mut self, item_id: i32, count: i16) -> i16 {
        if item_id == AIR_ITEM || count <= 0 {
            return 0;
        }
        let max = max_stack(item_id).max(1);
        let mut left = count;
        let storage = || HOTBAR.chain(MAIN);
        for i in storage().chain([OFFHAND]) {
            let s = &mut self.slots[i];
            if left > 0 && !s.is_empty() && s.item_id == item_id && s.count < max {
                let moved = left.min(max - s.count);
                s.count += moved;
                left -= moved;
            }
        }
        for i in storage() {
            let s = &mut self.slots[i];
            if left > 0 && s.is_empty() {
                let moved = left.min(max);
                *s = Slot {
                    count: moved,
                    item_id,
                };
                left -= moved;
            }
        }
        left
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vanilla_items::item;

    #[test]
    fn set_and_get() {
        let mut inv = Inventory::default();
        inv.set(36, 5, item::DIRT);
        assert_eq!(
            inv.get(36),
            Slot {
                count: 5,
                item_id: item::DIRT
            }
        );
        inv.set(36, 0, item::DIRT);
        assert_eq!(inv.get(36), Slot::default());
        inv.set(37, 3, AIR_ITEM);
        assert_eq!(inv.get(37), Slot::default());
        inv.set(INVENTORY_SIZE, 1, item::DIRT);
        assert_eq!(inv.get(INVENTORY_SIZE), Slot::default());
    }

    #[test]
    fn insert_tops_up_then_fills_hotbar_first() {
        let mut inv = Inventory::default();
        inv.set(20, 60, item::COBBLESTONE);
        assert_eq!(inv.insert(item::COBBLESTONE, 10), 0);
        assert_eq!(inv.get(20).count, 64);
        assert_eq!(
            inv.get(36),
            Slot {
                count: 6,
                item_id: item::COBBLESTONE
            }
        );
        assert_eq!(inv.insert(item::COBBLESTONE, 130), 0);
        assert_eq!(inv.get(36).count, 64);
        assert_eq!(inv.get(37).count, 64);
        assert_eq!(inv.get(38).count, 8);
        assert_eq!(inv.insert(AIR_ITEM, 5), 0);
        assert_eq!(inv.insert(item::DIRT, 0), 0);
    }

    #[test]
    fn insert_respects_max_stack() {
        let mut inv = Inventory::default();
        assert_eq!(inv.insert(item::ENDER_PEARL, 20), 0);
        assert_eq!(inv.get(36).count, 16);
        assert_eq!(inv.get(37).count, 4);
        assert_eq!(inv.insert(item::WOODEN_PICKAXE, 2), 0);
        assert_eq!(
            inv.get(38),
            Slot {
                count: 1,
                item_id: item::WOODEN_PICKAXE
            }
        );
        assert_eq!(
            inv.get(39),
            Slot {
                count: 1,
                item_id: item::WOODEN_PICKAXE
            }
        );
        // Main inventory comes after the hotbar.
        assert_eq!(inv.insert(item::STICK, 64 * 5 + 1), 0);
        assert_eq!(inv.get(44).count, 64);
        assert_eq!(inv.get(9).count, 1);
    }

    #[test]
    fn insert_returns_leftover_when_full() {
        let mut inv = Inventory::default();
        for slot in 9..45 {
            inv.set(slot, 1, item::WOODEN_SWORD);
        }
        inv.set(44, 62, item::DIRT);
        assert_eq!(inv.insert(item::DIRT, 5), 3);
        assert_eq!(inv.get(44).count, 64);
        assert_eq!(inv.insert(item::WOODEN_SWORD, 1), 1);
        // Crafting, armor and offhand slots are never filled.
        assert!((0..9).chain([45]).all(|s| inv.get(s).is_empty()));
    }
}
