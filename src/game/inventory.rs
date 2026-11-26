use super::{Hand, ItemStack};

pub const SLOT_COUNT: usize = 46;
const CRAFT_OUT: usize = 0;
const CRAFT_IN: std::ops::Range<usize> = 1..5;
const ARMOR: std::ops::Range<usize> = 5..9;
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
        Self {
            slots: [None; SLOT_COUNT],
            carried: None,
            selected: 0,
            state_id: 0,
        }
    }
}
impl Inventory {
    pub fn from_persisted(slots: [Option<ItemStack>; SLOT_COUNT], selected: u8) -> Self {
        Self {
            slots,
            carried: None,
            selected: selected.min(8),
            state_id: 0,
        }
    }
    pub fn slots(&self) -> &[Option<ItemStack>; SLOT_COUNT] {
        &self.slots
    }
    pub fn carried(&self) -> Option<ItemStack> {
        self.carried
    }
    pub fn set_slot(&mut self, slot: usize, item: Option<ItemStack>) {
        if slot < SLOT_COUNT {
            self.slots[slot] = item;
            self.changed();
        }
    }
    pub fn set_carried(&mut self, item: Option<ItemStack>) {
        self.carried = item;
        self.changed();
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
    pub fn damage_hand(&mut self, hand: Hand, amount: u32) -> ClickResult {
        let slot = match hand {
            Hand::Main => 36 + usize::from(self.selected),
            Hand::Off => OFFHAND,
        };
        let Some(mut stack) = self.slots[slot] else {
            return ClickResult::Unchanged;
        };
        if stack.definition().max_damage == 0 || amount == 0 {
            return ClickResult::Unchanged;
        }
        let mut broke = false;
        for _ in 0..amount {
            if stack.damage_one() {
                broke = true;
                break;
            }
        }
        if broke {
            self.slots[slot] = None;
        } else {
            self.slots[slot] = Some(stack);
        }
        self.changed();
        ClickResult::Changed
    }
    /// Take and clear the cursor-carried item stack, if any.
    pub fn take_carried(&mut self) -> Option<ItemStack> {
        let carried = self.carried.take()?;
        self.changed();
        Some(carried)
    }
    /// Drain all non-empty items (crafting grid, armor, main, hotbar, offhand, carried) for death drops.
    pub fn drain_all(&mut self) -> Vec<ItemStack> {
        let mut out = Vec::new();
        self.slots[CRAFT_OUT] = None;
        for slot in self.slots[1..].iter_mut() {
            if let Some(stack) = slot.take() {
                out.push(stack);
            }
        }
        if let Some(carried) = self.carried.take() {
            out.push(carried);
        }
        if !out.is_empty() {
            self.changed();
        }
        out
    }
    /// Drain the 2x2 inventory crafting grid (slots 1..=4) and clear output slot 0.
    pub fn drain_crafting_grid(&mut self) -> Vec<ItemStack> {
        let mut out = Vec::new();
        for slot in &mut self.slots[CRAFT_IN] {
            if let Some(stack) = slot.take() {
                out.push(stack);
            }
        }
        self.slots[CRAFT_OUT] = None;
        if !out.is_empty() {
            self.changed();
        }
        out
    }
    fn refresh_crafting_result(&mut self) {
        self.slots[CRAFT_OUT] = crate::game::match_crafting(2, 2, &self.slots[CRAFT_IN]);
    }
    fn consume_crafting_inputs(&mut self) {
        for slot in &mut self.slots[CRAFT_IN] {
            if let Some(mut stack) = *slot {
                if stack.count > 1 {
                    stack.count -= 1;
                    *slot = Some(stack);
                } else {
                    *slot = crate::game::container_remainder(stack.item);
                }
            }
        }
        self.refresh_crafting_result();
    }
    fn can_fit_full_stack(&self, stack: ItemStack) -> bool {
        let max_stack = stack.definition().max_stack;
        let mut remaining = stack.count;
        for slot in HOTBAR.chain(MAIN) {
            if remaining == 0 {
                return true;
            }
            match self.slots[slot] {
                None => return true,
                Some(existing)
                    if existing.item == stack.item
                        && existing.damage == stack.damage
                        && existing.count < max_stack =>
                {
                    remaining = remaining.saturating_sub(max_stack - existing.count);
                }
                _ => {}
            }
        }
        remaining == 0
    }
    /// Insert an `ItemStack` preserving its `damage` component into hotbar then main inventory.
    /// Returns the number of items actually inserted (`0..=stack.count`).
    pub fn give_stack(&mut self, stack: ItemStack) -> u8 {
        let max_stack = stack.definition().max_stack;
        if max_stack == 0 || stack.count == 0 {
            return 0;
        }
        let mut remaining = stack.count;
        for slot in HOTBAR.chain(MAIN) {
            if remaining == 0 {
                break;
            }
            if let Some(mut existing) = self.slots[slot]
                && existing.item == stack.item
                && existing.damage == stack.damage
                && existing.count < max_stack
            {
                let add = (max_stack - existing.count).min(remaining);
                existing.count += add;
                remaining -= add;
                self.slots[slot] = Some(existing);
            }
        }
        for slot in HOTBAR.chain(MAIN) {
            if remaining == 0 {
                break;
            }
            if self.slots[slot].is_none() {
                let add = max_stack.min(remaining);
                self.slots[slot] = ItemStack::with_damage(stack.item, add, stack.damage);
                remaining -= add;
            }
        }
        let added = stack.count - remaining;
        if added > 0 {
            self.changed();
        }
        added
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

    /// Add up to `count` undamaged items into hotbar then main inventory.
    /// Returns the number of items actually inserted.
    pub fn give(&mut self, item: i32, count: u32) -> u32 {
        let Some(definition) = super::item_by_id(item) else {
            return 0;
        };
        let max_stack = u32::from(definition.max_stack);
        if max_stack == 0 || count == 0 {
            return 0;
        }
        let mut remaining = count;
        for slot in HOTBAR.chain(MAIN) {
            if remaining == 0 {
                break;
            }
            if let Some(mut stack) = self.slots[slot]
                && stack.item == item
                && stack.damage == 0
                && u32::from(stack.count) < max_stack
            {
                let add = (max_stack - u32::from(stack.count)).min(remaining);
                stack.count += add as u8;
                remaining -= add;
                self.slots[slot] = Some(stack);
            }
        }
        for slot in HOTBAR.chain(MAIN) {
            if remaining == 0 {
                break;
            }
            if self.slots[slot].is_none() {
                let add = max_stack.min(remaining);
                self.slots[slot] = ItemStack::new(item, add as u8);
                remaining -= add;
            }
        }
        let added = count - remaining;
        if added > 0 {
            self.changed();
        }
        added
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
        self.refresh_crafting_result();
        if before == (self.slots, self.carried) {
            ClickResult::Unchanged
        } else {
            self.changed();
            ClickResult::Changed
        }
    }
    fn storage(slot: usize) -> bool {
        CRAFT_IN.contains(&slot)
            || ARMOR.contains(&slot)
            || MAIN.contains(&slot)
            || HOTBAR.contains(&slot)
            || slot == OFFHAND
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
        // Clicking the 2x2 crafting output slot (0) crafts into the cursor.
        if slot == CRAFT_OUT as i16 {
            if !matches!(button, 0 | 1) {
                return false;
            }
            let Some(result) = self.slots[CRAFT_OUT] else {
                return false;
            };
            match self.carried {
                None => {
                    self.carried = Some(result);
                    self.consume_crafting_inputs();
                    return true;
                }
                Some(mut held)
                    if held.item == result.item
                        && held.damage == result.damage
                        && u16::from(held.count) + u16::from(result.count)
                            <= u16::from(result.definition().max_stack) =>
                {
                    held.count += result.count;
                    self.carried = Some(held);
                    self.consume_crafting_inputs();
                    return true;
                }
                _ => return false,
            }
        }
        let Some(slot) = Self::index(slot) else {
            return false;
        };
        match button {
            0 => {
                match (self.carried, self.slots[slot]) {
                    (Some(mut held), Some(mut there))
                        if held.item == there.item
                            && held.damage == there.damage
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
                        self.carried = Some(ItemStack {
                            count: take,
                            ..there
                        });
                        self.slots[slot] = (there.count > 0).then_some(there);
                    }
                    (Some(mut held), None) => {
                        self.slots[slot] = Some(ItemStack { count: 1, ..held });
                        held.count -= 1;
                        self.carried = (held.count > 0).then_some(held);
                    }
                    (Some(mut held), Some(mut there))
                        if held.item == there.item
                            && held.damage == there.damage
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
        // Shift-click on crafting output (slot 0) crafts as many as fit.
        if slot == CRAFT_OUT as i16 {
            let mut crafted = false;
            for _ in 0..64 {
                let Some(result) = self.slots[CRAFT_OUT] else {
                    break;
                };
                if !self.can_fit_full_stack(result) {
                    break;
                }
                self.give_stack(result);
                self.consume_crafting_inputs();
                crafted = true;
            }
            return crafted;
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
                && there.damage == stack.damage
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
                && stack.damage == carried.damage
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
    use super::super::item_by_name;
    use super::*;
    #[test]
    fn authoritative_clicks_move_split_merge_and_reject() {
        let mut i = Inventory::default();
        assert!(i.slots().iter().all(Option::is_none));
        let stone = item_by_name("stone").unwrap().id;
        i.set_slot(36, ItemStack::new(stone, 64));
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
                slot: 9,
                button: 1,
                mode: 0
            }),
            ClickResult::Changed
        );
        assert_eq!(i.slots[9].unwrap().count, 1);
        assert_eq!(
            i.click(InventoryClick {
                slot: 9,
                button: 0,
                mode: 0
            }),
            ClickResult::Changed
        );
        assert_eq!(i.slots[9].unwrap().count, 32);
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
        let glass = item_by_name("glass").unwrap().id;
        assert_eq!(
            i.set_creative(45, ItemStack::new(glass, 2)),
            ClickResult::Changed
        );
        assert_eq!(i.hand(Hand::Off).unwrap().count, 2);
        assert_eq!(i.swap_hands(), ClickResult::Changed);
        assert_eq!(i.hand(Hand::Main).unwrap().item, glass);
        assert_eq!(i.drop_hand(Hand::Main, false), ClickResult::Changed);
        assert_eq!(i.hand(Hand::Main).unwrap().count, 1);

        let pickaxe = item_by_name("wooden_pickaxe").unwrap().id;
        assert_eq!(
            i.set_creative(9, ItemStack::with_damage(pickaxe, 1, 10)),
            ClickResult::Changed
        );
        assert_eq!(
            i.click(InventoryClick {
                slot: 9,
                button: 1,
                mode: 0
            }),
            ClickResult::Changed
        );
        assert_eq!(i.carried.unwrap().damage, 10);
        assert_eq!(
            i.click(InventoryClick {
                slot: 10,
                button: 1,
                mode: 0
            }),
            ClickResult::Changed
        );
        assert_eq!(i.slots[10].unwrap().damage, 10);
    }
}
