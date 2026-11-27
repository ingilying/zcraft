//! Pure gameplay container state machines (Chest, 3x3 Crafting Table, Furnace)
//! and slot layout helpers, decoupled from network packet handlers.
use super::{
    Inventory, InventoryClick, ItemStack, container_remainder, fuel_burn_ticks, match_crafting,
    match_smelting,
};
use crate::world::{CHEST_SLOTS, FurnaceData};

pub fn container_slots(
    chest: &[Option<ItemStack>; CHEST_SLOTS],
    inv: &Inventory,
) -> Vec<Option<ItemStack>> {
    let mut slots = Vec::with_capacity(CHEST_SLOTS + 36);
    slots.extend_from_slice(chest);
    slots.extend_from_slice(&inv.slots()[9..45]);
    slots
}

pub fn crafting_table_slots(
    output: Option<ItemStack>,
    grid: &[Option<ItemStack>; 9],
    inv: &Inventory,
) -> Vec<Option<ItemStack>> {
    let mut slots = Vec::with_capacity(46);
    slots.push(output);
    slots.extend_from_slice(grid);
    slots.extend_from_slice(&inv.slots()[9..45]);
    slots
}

pub fn furnace_slots(furnace: &FurnaceData, inv: &Inventory) -> Vec<Option<ItemStack>> {
    let mut slots = Vec::with_capacity(39);
    slots.extend_from_slice(&furnace.slots());
    slots.extend_from_slice(&inv.slots()[9..45]);
    slots
}

pub fn preview_player_drop(inv: &Inventory, click: InventoryClick) -> Option<ItemStack> {
    if click.mode == 0 && click.slot == -999 {
        let mut carried = inv.carried()?;
        match click.button {
            0 => Some(carried),
            1 => {
                carried.count = 1;
                Some(carried)
            }
            _ => None,
        }
    } else if click.mode == 4 {
        let slot = usize::try_from(click.slot).ok()?;
        let mut stack = (*inv.slots().get(slot)?)?;
        match click.button {
            0 => {
                stack.count = 1;
                Some(stack)
            }
            1 => Some(stack),
            _ => None,
        }
    } else {
        None
    }
}

pub fn apply_chest_click(
    chest: &mut [Option<ItemStack>; CHEST_SLOTS],
    inv: &mut Inventory,
    click: InventoryClick,
) -> (bool, Option<ItemStack>) {
    // Drop cursor item when clicking outside (-999) in mode 0.
    if click.mode == 0 && click.slot == -999 {
        let Some(mut car) = inv.carried() else {
            return (false, None);
        };
        return match click.button {
            0 => {
                inv.set_carried(None);
                (true, Some(car))
            }
            1 => {
                let mut one = car;
                one.count = 1;
                car.count -= 1;
                inv.set_carried((car.count > 0).then_some(car));
                (true, Some(one))
            }
            _ => (false, None),
        };
    }

    let Ok(slot) = usize::try_from(click.slot) else {
        return (false, None);
    };
    let total = CHEST_SLOTS + 36;
    if slot >= total {
        return (false, None);
    }

    let get_slot = |chest: &[Option<ItemStack>; CHEST_SLOTS], inv: &Inventory, idx: usize| {
        if idx < CHEST_SLOTS {
            chest[idx]
        } else {
            inv.slots()[9 + (idx - CHEST_SLOTS)]
        }
    };
    let set_slot = |chest: &mut [Option<ItemStack>; CHEST_SLOTS],
                    inv: &mut Inventory,
                    idx: usize,
                    val: Option<ItemStack>| {
        if idx < CHEST_SLOTS {
            chest[idx] = val;
        } else {
            inv.set_slot(9 + (idx - CHEST_SLOTS), val);
        }
    };

    // Mode 2: Number-key hotbar swap (0..=8) or offhand swap (40).
    if click.mode == 2 {
        let target_inv = match click.button {
            0..=8 => 36 + click.button as usize,
            40 => 45,
            _ => return (false, None),
        };
        let a = get_slot(chest, inv, slot);
        let b = inv.slots()[target_inv];
        set_slot(chest, inv, slot, b);
        inv.set_slot(target_inv, a);
        return (true, None);
    }

    // Mode 4: Drop key (Q = 0 drops 1, Ctrl+Q = 1 drops full stack).
    if click.mode == 4 {
        let Some(mut cur) = get_slot(chest, inv, slot) else {
            return (false, None);
        };
        return match click.button {
            0 => {
                let mut one = cur;
                one.count = 1;
                cur.count -= 1;
                set_slot(chest, inv, slot, (cur.count > 0).then_some(cur));
                (true, Some(one))
            }
            1 => {
                set_slot(chest, inv, slot, None);
                (true, Some(cur))
            }
            _ => (false, None),
        };
    }

    // Mode 6: Double-click collect matching items into carried stack.
    if click.mode == 6 {
        if click.button != 0 {
            return (false, None);
        }
        let Some(mut carried) = inv.carried() else {
            return (false, None);
        };
        let max_stack = carried.definition().max_stack;
        let mut changed = false;
        for idx in 0..total {
            if carried.count >= max_stack {
                break;
            }
            if let Some(mut cur) = get_slot(chest, inv, idx)
                && cur.item == carried.item
                && cur.damage == carried.damage
            {
                let moved = (max_stack - carried.count).min(cur.count);
                carried.count += moved;
                cur.count -= moved;
                set_slot(chest, inv, idx, (cur.count > 0).then_some(cur));
                changed = true;
            }
        }
        if changed {
            inv.set_carried(Some(carried));
        }
        return (changed, None);
    }

    // Shift-click (mode 1): quick-move between chest (0..27) and player inventory (27..63).
    if click.mode == 1 {
        let Some(mut moving) = get_slot(chest, inv, slot) else {
            return (false, None);
        };
        let max_stack = moving.definition().max_stack;
        if slot < CHEST_SLOTS {
            for target in (9..45).rev() {
                if let Some(mut existing) = inv.slots()[target]
                    && existing.item == moving.item
                    && existing.damage == moving.damage
                    && existing.count < max_stack
                {
                    let add = (max_stack - existing.count).min(moving.count);
                    existing.count += add;
                    moving.count -= add;
                    inv.set_slot(target, Some(existing));
                    if moving.count == 0 {
                        chest[slot] = None;
                        return (true, None);
                    }
                }
            }
            for target in (9..45).rev() {
                if inv.slots()[target].is_none() {
                    inv.set_slot(target, Some(moving));
                    chest[slot] = None;
                    return (true, None);
                }
            }
            chest[slot] = Some(moving);
            return (true, None);
        } else {
            let inv_idx = 9 + (slot - CHEST_SLOTS);
            for slot_ref in chest.iter_mut() {
                if let Some(mut existing) = *slot_ref
                    && existing.item == moving.item
                    && existing.damage == moving.damage
                    && existing.count < max_stack
                {
                    let add = (max_stack - existing.count).min(moving.count);
                    existing.count += add;
                    moving.count -= add;
                    *slot_ref = Some(existing);
                    if moving.count == 0 {
                        inv.set_slot(inv_idx, None);
                        return (true, None);
                    }
                }
            }
            for slot_ref in chest.iter_mut() {
                if slot_ref.is_none() {
                    *slot_ref = Some(moving);
                    inv.set_slot(inv_idx, None);
                    return (true, None);
                }
            }
            inv.set_slot(inv_idx, Some(moving));
            return (true, None);
        }
    }

    if click.mode != 0 || !matches!(click.button, 0 | 1) {
        return (false, None);
    }

    let current = get_slot(chest, inv, slot);
    let carried = inv.carried();
    let (next_slot, next_carried) = merge_or_swap(click.button, current, carried);
    set_slot(chest, inv, slot, next_slot);
    inv.set_carried(next_carried);
    (true, None)
}

pub fn apply_crafting_table_click(
    grid: &mut [Option<ItemStack>; 9],
    output: &mut Option<ItemStack>,
    inv: &mut Inventory,
    click: InventoryClick,
) -> Option<ItemStack> {
    if click.mode == 0 && click.slot == -999 {
        let mut car = inv.carried()?;
        return match click.button {
            0 => {
                inv.set_carried(None);
                Some(car)
            }
            1 => {
                let mut one = car;
                one.count = 1;
                car.count -= 1;
                inv.set_carried((car.count > 0).then_some(car));
                Some(one)
            }
            _ => None,
        };
    }

    let slot = usize::try_from(click.slot).ok()?;
    if slot >= 46 {
        return None;
    }

    let consume_one = |grid: &mut [Option<ItemStack>; 9], output: &mut Option<ItemStack>| {
        for cell in grid.iter_mut() {
            if let Some(mut s) = *cell {
                if s.count > 1 {
                    s.count -= 1;
                    *cell = Some(s);
                } else {
                    *cell = container_remainder(s.item);
                }
            }
        }
        *output = match_crafting(3, 3, grid);
    };

    // Slot 0 = crafting output.
    if slot == 0 {
        if click.mode == 1 {
            for _ in 0..64 {
                let Some(res) = *output else {
                    break;
                };
                let added = inv.give_stack(res);
                if added < res.count {
                    break;
                }
                consume_one(grid, output);
            }
            return None;
        }
        if click.mode == 0 && matches!(click.button, 0 | 1) {
            let res = (*output)?;
            match inv.carried() {
                None => {
                    inv.set_carried(Some(res));
                    consume_one(grid, output);
                }
                Some(mut held)
                    if held.item == res.item
                        && held.damage == res.damage
                        && u16::from(held.count) + u16::from(res.count)
                            <= u16::from(res.definition().max_stack) =>
                {
                    held.count += res.count;
                    inv.set_carried(Some(held));
                    consume_one(grid, output);
                }
                _ => {}
            }
        }
        return None;
    }

    let get_s = |grid: &[Option<ItemStack>; 9], inv: &Inventory, idx: usize| {
        if (1..=9).contains(&idx) {
            grid[idx - 1]
        } else {
            inv.slots()[9 + (idx - 10)]
        }
    };
    let set_s = |grid: &mut [Option<ItemStack>; 9],
                 inv: &mut Inventory,
                 idx: usize,
                 val: Option<ItemStack>| {
        if (1..=9).contains(&idx) {
            grid[idx - 1] = val;
        } else {
            inv.set_slot(9 + (idx - 10), val);
        }
    };

    let mut dropped = None;
    match click.mode {
        0 if matches!(click.button, 0 | 1) => {
            let cur = get_s(grid, inv, slot);
            let car = inv.carried();
            let (ns, nc) = merge_or_swap(click.button, cur, car);
            set_s(grid, inv, slot, ns);
            inv.set_carried(nc);
        }
        1 => {
            if let Some(mut moving) = get_s(grid, inv, slot) {
                if (1..=9).contains(&slot) {
                    let added = inv.give_stack(moving);
                    moving.count -= added;
                    set_s(grid, inv, slot, (moving.count > 0).then_some(moving));
                } else {
                    let targets = if slot < 37 { 37..46 } else { 10..37 };
                    let max_stack = moving.definition().max_stack;
                    for t in targets.clone() {
                        if let Some(mut ex) = get_s(grid, inv, t)
                            && ex.item == moving.item
                            && ex.damage == moving.damage
                            && ex.count < max_stack
                        {
                            let add = (max_stack - ex.count).min(moving.count);
                            ex.count += add;
                            moving.count -= add;
                            set_s(grid, inv, t, Some(ex));
                            if moving.count == 0 {
                                break;
                            }
                        }
                    }
                    if moving.count > 0 {
                        for t in targets {
                            if get_s(grid, inv, t).is_none() {
                                set_s(grid, inv, t, Some(moving));
                                moving.count = 0;
                                break;
                            }
                        }
                    }
                    set_s(grid, inv, slot, (moving.count > 0).then_some(moving));
                }
            }
        }
        2 => {
            let target_inv = match click.button {
                0..=8 => Some(36 + click.button as usize),
                40 => Some(45),
                _ => None,
            };
            if let Some(tinv) = target_inv {
                let a = get_s(grid, inv, slot);
                let b = inv.slots()[tinv];
                set_s(grid, inv, slot, b);
                inv.set_slot(tinv, a);
            }
        }
        4 => {
            if let Some(mut cur) = get_s(grid, inv, slot) {
                match click.button {
                    0 => {
                        let mut one = cur;
                        one.count = 1;
                        cur.count -= 1;
                        set_s(grid, inv, slot, (cur.count > 0).then_some(cur));
                        dropped = Some(one);
                    }
                    1 => {
                        set_s(grid, inv, slot, None);
                        dropped = Some(cur);
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    *output = match_crafting(3, 3, grid);
    dropped
}

pub fn apply_furnace_click(
    furnace: &mut FurnaceData,
    inv: &mut Inventory,
    click: InventoryClick,
) -> (bool, Option<ItemStack>) {
    if click.mode == 0 && click.slot == -999 {
        let Some(mut car) = inv.carried() else {
            return (false, None);
        };
        return match click.button {
            0 => {
                inv.set_carried(None);
                (true, Some(car))
            }
            1 => {
                let mut one = car;
                one.count = 1;
                car.count -= 1;
                inv.set_carried((car.count > 0).then_some(car));
                (true, Some(one))
            }
            _ => (false, None),
        };
    }

    let Ok(slot) = usize::try_from(click.slot) else {
        return (false, None);
    };
    if slot >= 39 {
        return (false, None);
    }

    let get_s = |f: &FurnaceData, inv: &Inventory, idx: usize| match idx {
        0 => f.input,
        1 => f.fuel,
        2 => f.output,
        _ => inv.slots()[9 + (idx - 3)],
    };
    let set_s =
        |f: &mut FurnaceData, inv: &mut Inventory, idx: usize, val: Option<ItemStack>| match idx {
            0 => f.input = val,
            1 => f.fuel = val,
            2 => f.output = val,
            _ => inv.set_slot(9 + (idx - 3), val),
        };

    // Output slot 2: only take out, never place in.
    if slot == 2 {
        if click.mode == 1 {
            let Some(mut out) = furnace.output else {
                return (false, None);
            };
            let added = inv.give_stack(out);
            if added == 0 {
                return (false, None);
            }
            out.count -= added;
            furnace.output = (out.count > 0).then_some(out);
            return (true, None);
        }
        if click.mode == 0 && matches!(click.button, 0 | 1) {
            let Some(out) = furnace.output else {
                return (false, None);
            };
            return match inv.carried() {
                None => {
                    inv.set_carried(Some(out));
                    furnace.output = None;
                    (true, None)
                }
                Some(mut held)
                    if held.item == out.item
                        && held.damage == out.damage
                        && u16::from(held.count) + u16::from(out.count)
                            <= u16::from(out.definition().max_stack) =>
                {
                    held.count += out.count;
                    inv.set_carried(Some(held));
                    furnace.output = None;
                    (true, None)
                }
                _ => (false, None),
            };
        }
        return (false, None);
    }

    match click.mode {
        0 if matches!(click.button, 0 | 1) => {
            let cur = get_s(furnace, inv, slot);
            let car = inv.carried();
            // Restrict fuel slot (1) to valid fuel items or empty bucket.
            if slot == 1
                && let Some(c) = car
                && fuel_burn_ticks(c.item).is_none()
                && c.definition().name != "minecraft:bucket"
            {
                return (false, None);
            }
            let (ns, nc) = merge_or_swap(click.button, cur, car);
            set_s(furnace, inv, slot, ns);
            inv.set_carried(nc);
            (true, None)
        }
        1 => {
            let Some(mut moving) = get_s(furnace, inv, slot) else {
                return (false, None);
            };
            if slot < 3 {
                let added = inv.give_stack(moving);
                if added == 0 {
                    return (false, None);
                }
                moving.count -= added;
                set_s(furnace, inv, slot, (moving.count > 0).then_some(moving));
                (true, None)
            } else {
                let target_slot = if match_smelting(Some(moving)).is_some() {
                    Some(0)
                } else if fuel_burn_ticks(moving.item).is_some() {
                    Some(1)
                } else {
                    None
                };
                if let Some(ts) = target_slot {
                    let cur = get_s(furnace, inv, ts);
                    match cur {
                        None => {
                            set_s(furnace, inv, ts, Some(moving));
                            set_s(furnace, inv, slot, None);
                            return (true, None);
                        }
                        Some(mut ex)
                            if ex.item == moving.item
                                && ex.damage == moving.damage
                                && ex.count < ex.definition().max_stack =>
                        {
                            let add = (ex.definition().max_stack - ex.count).min(moving.count);
                            ex.count += add;
                            moving.count -= add;
                            set_s(furnace, inv, ts, Some(ex));
                            set_s(furnace, inv, slot, (moving.count > 0).then_some(moving));
                            return (true, None);
                        }
                        _ => {}
                    }
                }
                (false, None)
            }
        }
        2 => {
            let target_inv = match click.button {
                0..=8 => 36 + click.button as usize,
                40 => 45,
                _ => return (false, None),
            };
            let b = inv.slots()[target_inv];
            if slot == 1
                && let Some(c) = b
                && fuel_burn_ticks(c.item).is_none()
            {
                return (false, None);
            }
            let a = get_s(furnace, inv, slot);
            set_s(furnace, inv, slot, b);
            inv.set_slot(target_inv, a);
            (true, None)
        }
        4 => {
            let Some(mut cur) = get_s(furnace, inv, slot) else {
                return (false, None);
            };
            match click.button {
                0 => {
                    let mut one = cur;
                    one.count = 1;
                    cur.count -= 1;
                    set_s(furnace, inv, slot, (cur.count > 0).then_some(cur));
                    (true, Some(one))
                }
                1 => {
                    set_s(furnace, inv, slot, None);
                    (true, Some(cur))
                }
                _ => (false, None),
            }
        }
        _ => (false, None),
    }
}

fn merge_or_swap(
    button: i8,
    current: Option<ItemStack>,
    carried: Option<ItemStack>,
) -> (Option<ItemStack>, Option<ItemStack>) {
    match (button, current, carried) {
        (0, Some(mut cur), Some(mut car)) if cur.item == car.item && cur.damage == car.damage => {
            let max_stack = cur.definition().max_stack;
            let add = (max_stack - cur.count).min(car.count);
            cur.count += add;
            car.count -= add;
            (Some(cur), (car.count > 0).then_some(car))
        }
        (0, cur, car) => (car, cur),
        (1, Some(mut cur), None) => {
            let take = cur.count.div_ceil(2);
            cur.count -= take;
            let taken = ItemStack::with_damage(cur.item, take, cur.damage);
            ((cur.count > 0).then_some(cur), taken)
        }
        (1, None, Some(mut car)) => {
            let placed = ItemStack::with_damage(car.item, 1, car.damage);
            car.count -= 1;
            (placed, (car.count > 0).then_some(car))
        }
        (1, Some(mut cur), Some(mut car))
            if cur.item == car.item
                && cur.damage == car.damage
                && cur.count < cur.definition().max_stack =>
        {
            cur.count += 1;
            car.count -= 1;
            (Some(cur), (car.count > 0).then_some(car))
        }
        (1, cur, car) => (car, cur),
        _ => (current, carried),
    }
}
