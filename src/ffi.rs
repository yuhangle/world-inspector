//! C FFI layer for the world-inspector library.
//! Allows C++ plugins to call into Rust without subprocess overhead.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;

use crate::{Item, PlayerInventory, WorldHandle};

// ── C-compatible structures ──

/// A single inventory item (C-compatible).
/// Strings are null-terminated, must be freed via wi_free_inventory.
#[repr(C)]
pub struct WiItem {
    pub slot: i32,
    pub name: *mut c_char,
    pub count: i32,
    pub damage: i32,
    pub tag_json: *mut c_char,  // JSON string of NBT tag, or NULL
}

/// An array of items (C-compatible).
#[repr(C)]
pub struct WiItemArray {
    pub items: *mut WiItem,
    pub count: i32,
}

/// Full inventory result (C-compatible).
/// Must be freed with wi_free_inventory.
#[repr(C)]
pub struct WiInventoryResult {
    pub inventory: WiItemArray,
    pub armor: WiItemArray,
    pub offhand: *mut WiItem,  // NULL if no offhand item
}

// ── Internal conversions ──

fn item_to_c(item: &Item) -> WiItem {
    let name = CString::new(item.name.clone()).unwrap_or_default();
    let tag_json = match &item.tag {
        Some(v) => CString::new(v.to_string()).unwrap_or_default(),
        None => CString::default(),
    };
    WiItem {
        slot: item.slot,
        name: name.into_raw(),
        count: item.count,
        damage: item.damage,
        tag_json: if item.tag.is_some() { tag_json.into_raw() } else { ptr::null_mut() },
    }
}

fn inventory_to_c(inv: &PlayerInventory) -> WiInventoryResult {
    // Inventory items
    let inv_count = inv.inventory.len();
    let mut inv_items: Vec<WiItem> = inv.inventory.iter().map(item_to_c).collect();
    let inv_ptr = if inv_count > 0 {
        let p = inv_items.as_mut_ptr();
        std::mem::forget(inv_items);  // ownership transferred to C
        p
    } else {
        ptr::null_mut()
    };

    // Armor items
    let armor_count = inv.armor.len();
    let mut armor_items: Vec<WiItem> = inv.armor.iter().map(item_to_c).collect();
    let armor_ptr = if armor_count > 0 {
        let p = armor_items.as_mut_ptr();
        std::mem::forget(armor_items);
        p
    } else {
        ptr::null_mut()
    };

    // Offhand
    let offhand_ptr = match &inv.offhand {
        Some(oh) => {
            let c_item = item_to_c(oh);
            let p = Box::into_raw(Box::new(c_item));
            p
        }
        None => ptr::null_mut(),
    };

    WiInventoryResult {
        inventory: WiItemArray { items: inv_ptr, count: inv_count as i32 },
        armor: WiItemArray { items: armor_ptr, count: armor_count as i32 },
        offhand: offhand_ptr,
    }
}

// ── FFI exports ──

/// Open a Bedrock world database.
/// Returns an opaque handle, or NULL on error.
/// Must be closed with wi_close.
#[no_mangle]
pub extern "C" fn wi_open(path: *const c_char) -> *mut WorldHandle {
    if path.is_null() {
        return ptr::null_mut();
    }
    let c_str = unsafe { CStr::from_ptr(path) };
    let path_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };

    match WorldHandle::open(path_str) {
        Ok(handle) => Box::into_raw(Box::new(handle)),
        Err(e) => {
            eprintln!("[wi_open] Error: {}", e);
            ptr::null_mut()
        }
    }
}

/// Close a previously opened world handle and free all associated resources.
/// Safe to call with NULL (no-op).
#[no_mangle]
pub extern "C" fn wi_close(handle: *mut WorldHandle) {
    if !handle.is_null() {
        unsafe { drop(Box::from_raw(handle)); }
    }
}

/// Get a player's inventory by their player key.
/// Returns a WiInventoryResult that must be freed with wi_free_inventory.
/// Returns NULL if the player is not found or an error occurs.
#[no_mangle]
pub extern "C" fn wi_get_inventory(handle: *mut WorldHandle, player_key: *const c_char) -> *mut WiInventoryResult {
    if handle.is_null() || player_key.is_null() {
        return ptr::null_mut();
    }

    let handle = unsafe { &mut *handle };
    let c_str = unsafe { CStr::from_ptr(player_key) };
    let key_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };

    match handle.get_player_inventory(key_str) {
        Ok(inv) => {
            let c_result = inventory_to_c(&inv);
            Box::into_raw(Box::new(c_result))
        }
        Err(e) => {
            eprintln!("[wi_get_inventory] Error for key '{}': {}", key_str, e);
            ptr::null_mut()
        }
    }
}

/// Get a player's inventory as a JSON string. Returns NULL on error.
/// The returned string must be freed with wi_free_string.
#[no_mangle]
pub extern "C" fn wi_get_inventory_json(world_path: *const c_char, player_key: *const c_char) -> *mut c_char {
    if world_path.is_null() || player_key.is_null() {
        return ptr::null_mut();
    }
    let w_path = match unsafe { CStr::from_ptr(world_path) }.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };
    let p_key = match unsafe { CStr::from_ptr(player_key) }.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };

    let mut handle = match WorldHandle::open(w_path) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[wi_open] Error: {}", e);
            return ptr::null_mut();
        }
    };

    match handle.get_player_inventory(p_key) {
        Ok(inv) => {
            match serde_json::to_string(&inv) {
                Ok(json_str) => {
                    match CString::new(json_str) {
                        Ok(cs) => cs.into_raw(),
                        Err(_) => ptr::null_mut(),
                    }
                }
                Err(_) => ptr::null_mut(),
            }
        }
        Err(e) => {
            eprintln!("[wi_get_inventory] Error: {}", e);
            ptr::null_mut()
        }
    }
}

/// Free a C string returned by wi_get_inventory_json.
#[no_mangle]
pub extern "C" fn wi_free_string(s: *mut c_char) {
    if !s.is_null() {
        unsafe { drop(CString::from_raw(s)); }
    }
}

/// Free a WiInventoryResult and all its contents (items, strings).
/// Safe to call with NULL (no-op).
#[no_mangle]
pub extern "C" fn wi_free_inventory(result: *mut WiInventoryResult) {
    if result.is_null() {
        return;
    }
    let result = unsafe { Box::from_raw(result) };

    // Free inventory items
    if !result.inventory.items.is_null() {
        let count = result.inventory.count as usize;
        let items = unsafe { Vec::from_raw_parts(result.inventory.items, count, count) };
        for item in items {
            if !item.name.is_null() {
                unsafe { drop(CString::from_raw(item.name)); }
            }
            if !item.tag_json.is_null() {
                unsafe { drop(CString::from_raw(item.tag_json)); }
            }
        }
    }

    // Free armor items
    if !result.armor.items.is_null() {
        let count = result.armor.count as usize;
        let items = unsafe { Vec::from_raw_parts(result.armor.items, count, count) };
        for item in items {
            if !item.name.is_null() {
                unsafe { drop(CString::from_raw(item.name)); }
            }
            if !item.tag_json.is_null() {
                unsafe { drop(CString::from_raw(item.tag_json)); }
            }
        }
    }

    // Free offhand
    if !result.offhand.is_null() {
        let item = unsafe { Box::from_raw(result.offhand) };
        if !item.name.is_null() {
            unsafe { drop(CString::from_raw(item.name)); }
        }
        if !item.tag_json.is_null() {
            unsafe { drop(CString::from_raw(item.tag_json)); }
        }
    }
}

/// List all player keys in the world database.
/// Returns an array of C strings. Must be freed with wi_free_string_array.
/// out_count receives the number of keys.
#[no_mangle]
pub extern "C" fn wi_list_player_keys(handle: *mut WorldHandle, out_count: *mut i32) -> *mut *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let handle = unsafe { &mut *handle };
    let keys = handle.list_player_keys();

    let count = keys.len();
    if !out_count.is_null() {
        unsafe { *out_count = count as i32; }
    }

    if count == 0 {
        return ptr::null_mut();
    }

    let mut c_strings: Vec<*mut c_char> = keys.into_iter()
        .map(|k| CString::new(k).unwrap_or_default().into_raw())
        .collect();

    let ptr = c_strings.as_mut_ptr();
    std::mem::forget(c_strings);  // ownership transferred to C
    ptr
}

/// Free a string array returned by wi_list_player_keys.
#[no_mangle]
pub extern "C" fn wi_free_string_array(arr: *mut *mut c_char, count: i32) {
    if arr.is_null() || count <= 0 {
        return;
    }
    let count = count as usize;
    let strings = unsafe { Vec::from_raw_parts(arr, count, count) };
    for s in strings {
        if !s.is_null() {
            unsafe { drop(CString::from_raw(s)); }
        }
    }
}

// ── Encoded inventory FFI (pre-serialized binary NBT for inventoryui) ──

/// A single inventory item with pre-serialized binary NBT (C-compatible).
#[repr(C)]
pub struct WiEncodedItem {
    pub slot: i32,
    pub type_id: *mut c_char,
    pub count: i32,
    pub damage: i32,
    pub nbt_bytes: *mut u8,
    pub nbt_len: i32,
}

/// An array of encoded items (C-compatible).
/// Must be freed with wi_free_encoded_inventory.
#[repr(C)]
pub struct WiEncodedInventory {
    pub items: *mut WiEncodedItem,
    pub count: i32,
}

#[no_mangle]
pub extern "C" fn wi_get_encoded_inventory(
    world_path: *const c_char,
    player_key: *const c_char,
) -> *mut WiEncodedInventory {
    if world_path.is_null() || player_key.is_null() {
        return ptr::null_mut();
    }
    let w_path = match unsafe { CStr::from_ptr(world_path) }.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };
    let p_key = match unsafe { CStr::from_ptr(player_key) }.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };

    let mut handle = match WorldHandle::open(w_path) {
        Ok(h) => h,
        Err(_) => return ptr::null_mut(),
    };

    let encoded = match handle.get_player_encoded_items(p_key) {
        Ok(items) => items,
        Err(_) => return ptr::null_mut(),
    };

    let count = encoded.len();
    if count == 0 {
        return Box::into_raw(Box::new(WiEncodedInventory { items: ptr::null_mut(), count: 0 }));
    }

    let mut c_items: Vec<WiEncodedItem> = Vec::with_capacity(count);
    for item in encoded {
        let type_id = CString::new(item.name).unwrap_or_default();
        let nbt_len = item.nbt_bytes.len() as i32;
        let nbt_ptr = if nbt_len > 0 {
            let mut buf = item.nbt_bytes.into_boxed_slice();
            let p = buf.as_mut_ptr();
            std::mem::forget(buf);
            p
        } else {
            ptr::null_mut()
        };
        c_items.push(WiEncodedItem {
            slot: item.slot,
            type_id: type_id.into_raw(),
            count: item.count,
            damage: item.damage,
            nbt_bytes: nbt_ptr,
            nbt_len,
        });
    }

    let ptr = c_items.as_mut_ptr();
    std::mem::forget(c_items);
    Box::into_raw(Box::new(WiEncodedInventory { items: ptr, count: count as i32 }))
}

#[no_mangle]
pub extern "C" fn wi_free_encoded_inventory(inv: *mut WiEncodedInventory) {
    if inv.is_null() { return; }
    let inv = unsafe { Box::from_raw(inv) };
    if inv.items.is_null() { return; }

    let count = inv.count as usize;
    let items = unsafe { Vec::from_raw_parts(inv.items, count, count) };
    for item in items {
        if !item.type_id.is_null() {
            unsafe { drop(CString::from_raw(item.type_id)); }
        }
        if !item.nbt_bytes.is_null() {
            unsafe { drop(Box::from_raw(std::slice::from_raw_parts_mut(item.nbt_bytes, item.nbt_len as usize))); }
        }
    }
}
