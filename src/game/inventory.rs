use super::{Hand, ItemKind, ItemStack};

pub const SLOT_COUNT: usize = 46;
const MAIN: std::ops::Range<usize> = 9..36;
const HOTBAR: std::ops::Range<usize> = 36..45;
const OFFHAND: usize = 45;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InventoryClick {
    pub slot: i16,
    pub button: i8,
    pub mode: i32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickResult {
    Changed,
    Unchanged,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inventory {
    slots: [Option<ItemStack>; SLOT_COUNT],
    carried: Option<ItemStack>,
    selected: u8,
    state_id: i32,
}
impl Default for Inventory {
    fn default() -> Self {
        let mut slots = [None; SLOT_COUNT];
        slots[36] = ItemStack::new(ItemKind::Stone, 64);
        slots[37] = ItemStack::new(ItemKind::Dirt, 64);
        slots[38] = ItemStack::new(ItemKind::Glass, 64);
        Self {
            slots,
            carried: None,
            selected: 0,
            state_id: 0,
        }
    }
}
impl Inventory {
    pub fn slots(&self) -> &[Option<ItemStack>; SLOT_COUNT] {
        &self.slots
    }
    pub fn carried(&self) -> Option<ItemStack> {
        self.carried
    }
    pub fn state_id(&self) -> i32 {
        self.state_id
    }
    pub fn selected(&self) -> u8 {
        self.selected
    }
    pub fn select(&mut self, slot: u8) {
        debug_assert!(slot < 9);
        self.selected = slot;
    }
    pub fn hand(&self, hand: Hand) -> Option<ItemStack> {
        match hand {
            Hand::Main => self.slots[36 + usize::from(self.selected)],
            Hand::Off => self.slots[OFFHAND],
        }
    }
    pub fn drop_hand(&mut self, hand: Hand, all: bool) -> ClickResult {
        let slot = match hand {
            Hand::Main => 36 + usize::from(self.selected),
            Hand::Off => OFFHAND,
        };
        let before = self.slots[slot];
        if let Some(mut stack) = before {
            if all || stack.count == 1 {
                self.slots[slot] = None
            } else {
                stack.count -= 1;
                self.slots[slot] = Some(stack)
            }
        }
        if self.slots[slot] == before {
            ClickResult::Unchanged
        } else {
            self.changed();
            ClickResult::Changed
        }
    }
    pub fn swap_hands(&mut self) -> ClickResult {
        let main = 36 + usize::from(self.selected);
        if self.slots[main] == self.slots[OFFHAND] {
            return ClickResult::Unchanged;
        }
        self.slots.swap(main, OFFHAND);
        self.changed();
        ClickResult::Changed
    }
    pub fn set_creative(&mut self, slot: i16, stack: Option<ItemStack>) -> ClickResult {
        let Ok(slot) = usize::try_from(slot) else {
            return ClickResult::Rejected;
        };
        if !Self::storage(slot) {
            return ClickResult::Rejected;
        }
        if self.slots[slot] == stack {
            return ClickResult::Unchanged;
        }
        self.slots[slot] = stack;
        self.changed();
        ClickResult::Changed
    }
    pub fn click(&mut self, click: InventoryClick) -> ClickResult {
        let before = (self.slots, self.carried);
        let valid = match click.mode {
            0 => self.pickup(click.slot, click.button),
            1 => self.quick_move(click.slot, click.button),
            2 => self.swap_hotbar(click.slot, click.button),
            4 => self.drop_slot(click.slot, click.button),
            6 => self.collect(click.slot, click.button),
            _ => false,
        };
        if !valid {
            self.slots = before.0;
            self.carried = before.1;
            return ClickResult::Rejected;
        }
        if before == (self.slots, self.carried) {
            ClickResult::Unchanged
        } else {
            self.changed();
            ClickResult::Changed
        }
    }
    fn storage(slot: usize) -> bool {
        MAIN.contains(&slot) || HOTBAR.contains(&slot) || slot == OFFHAND
    }
    fn index(slot: i16) -> Option<usize> {
        usize::try_from(slot)
            .ok()
            .filter(|slot| Self::storage(*slot))
    }
    fn changed(&mut self) {
        self.state_id = if self.state_id == i32::MAX {
            0
        } else {
            self.state_id + 1
        };
    }
    fn pickup(&mut self, slot: i16, button: i8) -> bool {
        if slot == -999 {
            return match button {
                0 => {
                    self.carried = None;
                    true
                }
                1 => {
                    if let Some(mut stack) = self.carried {
                        stack.count -= 1;
                        self.carried = (stack.count > 0).then_some(stack);
                    }
                    true
                }
                _ => false,
            };
        }
        let Some(slot) = Self::index(slot) else {
            return false;
        };
        match button {
            0 => {
                match (self.carried, self.slots[slot]) {
                    (Some(mut held), Some(mut there))
                        if held.item == there.item
                            && there.count < there.definition().max_stack =>
                    {
                        let moved = (there.definition().max_stack - there.count).min(held.count);
                        there.count += moved;
                        held.count -= moved;
                        self.slots[slot] = Some(there);
                        self.carried = (held.count > 0).then_some(held);
                    }
                    _ => std::mem::swap(&mut self.carried, &mut self.slots[slot]),
                }
                true
            }
            1 => {
                match (self.carried, self.slots[slot]) {
                    (None, Some(mut there)) => {
                        let take = there.count.div_ceil(2);
                        there.count -= take;
                        self.carried = ItemStack::new(there.item, take);
                        self.slots[slot] = (there.count > 0).then_some(there);
                    }
                    (Some(mut held), None) => {
                        self.slots[slot] = ItemStack::new(held.item, 1);
                        held.count -= 1;
                        self.carried = (held.count > 0).then_some(held);
                    }
                    (Some(mut held), Some(mut there))
                        if held.item == there.item
                            && there.count < there.definition().max_stack =>
                    {
                        there.count += 1;
                        held.count -= 1;
                        self.slots[slot] = Some(there);
                        self.carried = (held.count > 0).then_some(held);
                    }
                    _ => std::mem::swap(&mut self.carried, &mut self.slots[slot]),
                }
                true
            }
            _ => false,
        }
    }
    fn quick_move(&mut self, slot: i16, button: i8) -> bool {
        if !matches!(button, 0 | 1) {
            return false;
        }
        let Some(source) = Self::index(slot) else {
            return false;
        };
        let targets = if MAIN.contains(&source) {
            HOTBAR.clone()
        } else {
            MAIN.clone()
        };
        let Some(mut stack) = self.slots[source] else {
            return true;
        };
        for target in targets.clone() {
            if let Some(mut there) = self.slots[target]
                && there.item == stack.item
                && there.count < there.definition().max_stack
            {
                let moved = (there.definition().max_stack - there.count).min(stack.count);
                there.count += moved;
                stack.count -= moved;
                self.slots[target] = Some(there);
                if stack.count == 0 {
                    break;
                }
            }
        }
        if stack.count > 0 {
            for target in targets {
                if self.slots[target].is_none() {
                    self.slots[target] = Some(stack);
                    stack.count = 0;
                    break;
                }
            }
        }
        self.slots[source] = (stack.count > 0).then_some(stack);
        true
    }
    fn swap_hotbar(&mut self, slot: i16, button: i8) -> bool {
        let Some(slot) = Self::index(slot) else {
            return false;
        };
        let target = match button {
            0..=8 => 36 + button as usize,
            40 => OFFHAND,
            _ => return false,
        };
        self.slots.swap(slot, target);
        true
    }
    fn drop_slot(&mut self, slot: i16, button: i8) -> bool {
        let Some(slot) = Self::index(slot) else {
            return false;
        };
        match button {
            0 => {
                if let Some(mut stack) = self.slots[slot] {
                    stack.count -= 1;
                    self.slots[slot] = (stack.count > 0).then_some(stack)
                }
            }
            1 => self.slots[slot] = None,
            _ => return false,
        }
        true
    }
    fn collect(&mut self, slot: i16, button: i8) -> bool {
        if button != 0 || slot < 0 {
            return false;
        }
        let Some(mut carried) = self.carried else {
            return true;
        };
        let max = carried.definition().max_stack;
        for slot in MAIN.chain(HOTBAR).chain(std::iter::once(OFFHAND)) {
            if carried.count == max {
                break;
            }
            if let Some(mut stack) = self.slots[slot]
                && stack.item == carried.item
            {
                let moved = (max - carried.count).min(stack.count);
                carried.count += moved;
                stack.count -= moved;
                self.slots[slot] = (stack.count > 0).then_some(stack)
            }
        }
        self.carried = Some(carried);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authoritative_clicks_move_split_merge_and_reject() {
        let mut i = Inventory::default();
        assert_eq!(
            i.click(InventoryClick {
                slot: 36,
                button: 1,
                mode: 0
            }),
            ClickResult::Changed
        );
        assert_eq!(i.carried.unwrap().count, 32);
        assert_eq!(
            i.click(InventoryClick {
                slot: 39,
                button: 1,
                mode: 0
            }),
            ClickResult::Changed
        );
        assert_eq!(i.slots[39].unwrap().count, 1);
        assert_eq!(
            i.click(InventoryClick {
                slot: 39,
                button: 0,
                mode: 0
            }),
            ClickResult::Changed
        );
        assert_eq!(i.slots[39].unwrap().count, 32);
        let before = i.clone();
        assert_eq!(
            i.click(InventoryClick {
                slot: 0,
                button: 0,
                mode: 0
            }),
            ClickResult::Rejected
        );
        assert_eq!(i, before);
        assert_eq!(
            i.set_creative(45, ItemStack::new(ItemKind::Glass, 2)),
            ClickResult::Changed
        );
        assert_eq!(i.hand(Hand::Off).unwrap().count, 2);
        assert_eq!(i.swap_hands(), ClickResult::Changed);
        assert_eq!(i.hand(Hand::Main).unwrap().item, ItemKind::Glass);
        assert_eq!(i.drop_hand(Hand::Main, false), ClickResult::Changed);
        assert_eq!(i.hand(Hand::Main).unwrap().count, 1);
    }
}
