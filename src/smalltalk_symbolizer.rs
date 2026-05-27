use std::collections::HashMap;

use proc_maps::MapRange;
use remoteprocess::ProcessMemory;

use crate::binary_parser::BinaryInfo;

const COG_METHOD_SIZE: u64 = 0x28;
const COG_METHOD_SELECTOR_OFFSET: u64 = 0x20;
const COG_METHOD_BLOCK_SIZE_OFFSET: u64 = 0x0c;
const COG_METHOD_METHOD_OBJECT_OFFSET: u64 = 0x10;
const COG_METHOD_METHOD_HEADER_OFFSET: u64 = 0x18;
const COG_METHOD_FLAGS_OFFSET: u64 = 0x08;
const COG_SCAN_LIMIT: u64 = 1024 * 1024;
const MAX_METHOD_ZONE_SIZE: u64 = 512 * 1024 * 1024;
const BASE_HEADER_SIZE: u64 = 8;
const WORD_SIZE: usize = 8;
const TAG_MASK: u64 = 0x7;
const FORMAT_FIELD_BYTE_OFFSET: u64 = 3;
const NUM_SLOTS_FIELD_BYTE_OFFSET: u64 = 7;
const FORMAT_MASK: u8 = 0x1f;
const FIRST_WORD_OR_BYTE_FORMAT: u8 = 9;
const FIRST_BYTE_FORMAT: u8 = 16;
const FIRST_COMPILED_METHOD_FORMAT: u8 = 24;
const LAST_POINTER_FORMAT: u8 = 5;
const NUM_SLOTS_MASK: u8 = 255;
const LITERAL_START: u64 = 1;
const HEADER_INDEX: u64 = 0;
const ALTERNATE_HEADER_NUM_LITERALS_MASK: u64 = 0x7fff;
const SMALL_INTEGER_TAG: u64 = 1;
const NIL_OBJECT: u64 = 0x19;
const CM_METHOD: u8 = 5;
const CM_METHOD_FLAGGED_FOR_BECOME: u8 = 6;
const CM_OPEN_PIC: u8 = 3;
const CM_CLOSED_PIC: u8 = 2;
const COG_CODE_WINDOW: u64 = 64 * 1024;
const MAX_JIT_ENTRY_SPAN: u64 = 4096;
const LOW_COG_CODE_MIN: u64 = 0x10000;
const LOW_COG_CODE_MAX: u64 = u32::MAX as u64;
const MAX_CLASS_NAME_DEPTH: u8 = 4;
const MAX_CLASS_LITERAL_SCAN: u64 = 4;
const MAX_CLASS_OBJECT_SCAN: u64 = 8;

#[derive(Clone, Copy)]
struct MethodZoneSymbols {
    base_address_addr: u64,
    free_start_addr: u64,
}

/// Addresses of VM globals needed to walk the Cog frame chain.
#[derive(Clone, Copy)]
struct CogFrameSymbols {
    frame_pointer_addr: u64,
    heap_base_addr: u64,
}

const COG_FRAME_WALK_LIMIT: usize = 200;

#[derive(Clone)]
struct JitEntrySymbolSlot {
    slot_addr: u64,
    name: String,
}

#[derive(Clone)]
struct JitEntrySymbol {
    address: u64,
    name: String,
}

#[derive(Clone)]
struct CogMethodRange {
    start: u64,
    end: u64,
    name: String,
}

pub struct SmalltalkSymbolizer {
    process: remoteprocess::Process,
    maps: Vec<MapRange>,
    method_ranges: Vec<CogMethodRange>,
    resolved_pcs: HashMap<u64, Option<String>>,
    method_zone_symbols: Option<MethodZoneSymbols>,
    method_zone_bounds: Option<(u64, u64)>,
    method_zone_scan_cursor: Option<u64>,
    method_zone_blocked_cursor: Option<u64>,
    jit_entry_symbol_slots: Vec<JitEntrySymbolSlot>,
    jit_entry_symbols: Vec<JitEntrySymbol>,
    cog_frame_symbols: Option<CogFrameSymbols>,
}

impl SmalltalkSymbolizer {
    pub fn new(
        pid: remoteprocess::Pid,
        _process: &remoteprocess::Process,
        binary: Option<&BinaryInfo>,
    ) -> SmalltalkSymbolizer {
        let maps = proc_maps::get_process_maps(pid).unwrap_or_default();
        let jit_entry_symbol_slots = binary
            .map(|binary| {
                binary
                    .symbols
                    .iter()
                    .filter(|(name, addr)| {
                        symbol_is_in_bss(binary, **addr) && looks_like_jit_entry_symbol(name)
                    })
                    .map(|(name, addr)| JitEntrySymbolSlot {
                        slot_addr: *addr,
                        name: clean_jit_entry_symbol_name(name),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let method_zone_symbols = binary.and_then(|binary| {
            let base_address_addr = binary
                .symbols
                .get("baseAddress")
                .or_else(|| binary.symbols.get("methodZoneBase"))?;
            let free_start_addr = binary.symbols.get("mzFreeStart")?;
            Some(MethodZoneSymbols {
                base_address_addr: *base_address_addr,
                free_start_addr: *free_start_addr,
            })
        });
        if let Some(symbols) = method_zone_symbols {
            debug!(
                "OpenSmalltalk Cog method-zone symbols: base @ 0x{:x}, freeStart @ 0x{:x}",
                symbols.base_address_addr, symbols.free_start_addr
            );
        } else {
            debug!("OpenSmalltalk Cog method-zone symbols were not found");
        }
        let cog_frame_symbols = binary.and_then(|binary| {
            let frame_pointer_addr = binary.symbols.get("framePointer")?;
            let heap_base_addr = binary.symbols.get("heapBase")?;
            Some(CogFrameSymbols {
                frame_pointer_addr: *frame_pointer_addr,
                heap_base_addr: *heap_base_addr,
            })
        });
        if let Some(symbols) = cog_frame_symbols {
            debug!(
                "OpenSmalltalk Cog frame symbols: framePointer @ 0x{:x}, heapBase @ 0x{:x}",
                symbols.frame_pointer_addr, symbols.heap_base_addr
            );
        } else {
            debug!("OpenSmalltalk Cog frame symbols were not found");
        }
        let process = remoteprocess::Process::new(pid)
            .expect("SmalltalkSymbolizer requires an already-open process");

        SmalltalkSymbolizer {
            process,
            maps,
            method_ranges: Vec::new(),
            resolved_pcs: HashMap::new(),
            method_zone_symbols,
            method_zone_bounds: None,
            method_zone_scan_cursor: None,
            method_zone_blocked_cursor: None,
            jit_entry_symbol_slots,
            jit_entry_symbols: Vec::new(),
            cog_frame_symbols,
        }
    }

    pub fn resolve_jit_pc(&mut self, pc: u64) -> Option<String> {
        if let Some(cached) = self.resolved_pcs.get(&pc) {
            return cached.clone();
        }

        if let Some(name) = self.lookup_method_range(pc) {
            self.resolved_pcs.insert(pc, Some(name.clone()));
            return Some(name);
        }

        let should_refresh = match self.method_zone_bounds {
            Some((base, free_start)) => {
                let scan_cursor = self.method_zone_scan_cursor.unwrap_or(base);
                pc >= free_start || (pc >= scan_cursor && scan_cursor < free_start)
            }
            None => true,
        };
        if should_refresh {
            self.refresh_method_zone();
            if let Some(name) = self.lookup_method_range(pc) {
                self.resolved_pcs.insert(pc, Some(name.clone()));
                return Some(name);
            }
        }

        let resolved = self
            .lookup_jit_entry_symbol(pc)
            .or_else(|| {
                if self.pc_in_method_zone(pc) || self.method_zone_bounds.is_none() {
                    self.scan_for_cog_method(pc)
                } else {
                    None
                }
            })
            .or_else(|| self.generic_cog_code_name(pc));
        self.resolved_pcs.insert(pc, resolved.clone());
        resolved
    }

    /// Walk the Cog internal frame chain starting from the VM's current `framePointer`.
    ///
    /// The Cog frame layout on x86-64 is:
    ///   FP[0]   = caller/sender FP (next frame up the chain, 0 = end)
    ///   FP[-8]  = method field:
    ///               if < heapBase → pointer into the Cog method zone (JIT'd CogMethod)
    ///               if >= heapBase → a Smalltalk context OOP (interpreted frame)
    ///
    /// For JIT frames, FP[-8] points directly at a CogMethod header, and we resolve
    /// the name via the method zone.  For interpreted frames, FP[-8] is a context
    /// whose `method` field (slot 3, offset 0x20 from OOP) is the CompiledMethod OOP;
    /// we read its selector from the literal frame.
    ///
    /// Returns frames in caller order (innermost first), suitable for splicing into
    /// a native stack trace.
    pub fn walk_cog_frames(&mut self) -> Vec<String> {
        let Some(symbols) = self.cog_frame_symbols else {
            return Vec::new();
        };

        let fp: u64 = match self.process.copy_struct(symbols.frame_pointer_addr as usize) {
            Ok(fp) => fp,
            Err(_) => return Vec::new(),
        };
        if fp == 0 {
            return Vec::new();
        }

        let heap_base: u64 = match self.process.copy_struct(symbols.heap_base_addr as usize) {
            Ok(hb) => hb,
            Err(_) => return Vec::new(),
        };

        let mut frames = Vec::new();
        let mut current_fp = fp;
        let mut iterations = 0;

        while current_fp != 0 && iterations < COG_FRAME_WALK_LIMIT {
            iterations += 1;

            // FP[-8] = method field
            let method_field: u64 = match self
                .process
                .copy_struct((current_fp.wrapping_sub(8)) as usize)
            {
                Ok(mf) => mf,
                Err(_) => break,
            };

            if let Some(name) = self.resolve_frame_method(method_field, heap_base) {
                frames.push(name);
            }

            // FP[0] = caller FP
            let caller_fp: u64 = match self.process.copy_struct(current_fp as usize) {
                Ok(cfp) => cfp,
                Err(_) => break,
            };

            // Safety: prevent infinite loops
            if caller_fp == current_fp || caller_fp == 0 {
                break;
            }
            current_fp = caller_fp;
        }

        frames
    }

    /// Resolve the method name from a Cog frame's method field.
    ///
    /// If the method field points below `heap_base`, it's a CogMethod pointer
    /// in the method zone — resolve it directly.
    ///
    /// If it points at or above `heap_base`, it's a Smalltalk context OOP.
    /// We read the context's method field (slot 3) and resolve the CompiledMethod.
    fn resolve_frame_method(&mut self, method_field: u64, heap_base: u64) -> Option<String> {
        if method_field == 0 {
            return None;
        }

        if method_field < heap_base {
            // JIT frame: method_field is a CogMethod* in the method zone.
            // The method's executable code starts at CogMethod + COG_METHOD_SIZE.
            // Use the entry point (just past the header) as the PC for resolution.
            let pc = method_field + COG_METHOD_SIZE;
            self.resolve_jit_pc(pc)
        } else {
            // Interpreted frame: method_field is a context OOP.
            // Context layout (Spur, 64-bit):
            //   slot 0 (oop+8)  = sender
            //   slot 1 (oop+16) = instructionPointer
            //   slot 2 (oop+24) = stackPointer
            //   slot 3 (oop+32) = method
            //   slot 4 (oop+40) = closureOrNil
            //   slot 5 (oop+48) = receiver
            if !is_heap_object(method_field) {
                return None;
            }
            let method_obj: u64 = self
                .process
                .copy_struct((method_field + BASE_HEADER_SIZE + 3 * WORD_SIZE as u64) as usize)
                .ok()?;
            if !is_heap_object(method_obj) || !self.is_compiled_method(method_obj) {
                return None;
            }
            let method_header = self.method_header_of(method_obj)?;
            let selector = self
                .maybe_selector_of_method(method_obj, method_header, 0)
                .filter(|s| looks_like_selector(s))?;
            let class_name = self.class_name_of_method(method_obj, method_header, 0);
            Some(match class_name {
                Some(cn) => format!("{cn}>>{selector}"),
                None => format!("???>>{selector}"),
            })
        }
    }

    fn lookup_method_range(&self, pc: u64) -> Option<String> {
        let index = self
            .method_ranges
            .partition_point(|range| range.start <= pc);
        if index == 0 {
            return None;
        }

        let range = &self.method_ranges[index - 1];
        if pc < range.end {
            Some(range.name.clone())
        } else {
            None
        }
    }

    fn insert_method_range(&mut self, range: CogMethodRange) {
        match self.method_ranges.last() {
            Some(last) if last.start == range.start => {
                *self.method_ranges.last_mut().expect("last method range") = range;
                return;
            }
            Some(last) if last.start < range.start => {
                self.method_ranges.push(range);
                return;
            }
            None => {
                self.method_ranges.push(range);
                return;
            }
            _ => {}
        }

        let index = self
            .method_ranges
            .partition_point(|existing| existing.start < range.start);
        if self
            .method_ranges
            .get(index)
            .is_some_and(|existing| existing.start == range.start)
        {
            self.method_ranges[index] = range;
        } else {
            self.method_ranges.insert(index, range);
        }
    }

    fn pc_in_method_zone(&self, pc: u64) -> bool {
        self.method_zone_bounds
            .is_some_and(|(base, free_start)| pc >= base && pc < free_start)
    }

    fn pc_looks_like_cog_code(&self, pc: u64) -> bool {
        let in_method_zone_window = self.method_zone_bounds.is_some_and(|(base, free_start)| {
            pc >= base.saturating_sub(COG_CODE_WINDOW)
                && pc < free_start.saturating_add(COG_CODE_WINDOW)
        });
        let in_low_cog_region = self.method_zone_bounds.is_some()
            && (LOW_COG_CODE_MIN..=LOW_COG_CODE_MAX).contains(&pc);

        in_method_zone_window
            || in_low_cog_region
            || self.maps.iter().any(|map| {
                let start = map.start() as u64;
                let end = start + map.size() as u64;
                pc >= start && pc < end && map.is_exec() && map.filename().is_none()
            })
    }

    fn generic_cog_code_name(&self, pc: u64) -> Option<String> {
        if self.pc_looks_like_cog_code(pc) {
            Some("JIT frame".to_owned())
        } else {
            None
        }
    }

    fn lookup_jit_entry_symbol(&self, pc: u64) -> Option<String> {
        let index = self
            .jit_entry_symbols
            .partition_point(|entry| entry.address <= pc);
        if index == 0 {
            return None;
        }

        let entry = &self.jit_entry_symbols[index - 1];
        let next_address = self
            .jit_entry_symbols
            .get(index)
            .map(|entry| entry.address)
            .unwrap_or_else(|| entry.address.saturating_add(MAX_JIT_ENTRY_SPAN));
        if pc >= entry.address
            && pc < next_address
            && pc - entry.address < MAX_JIT_ENTRY_SPAN
            && self.pc_looks_like_cog_code(pc)
        {
            Some(format!("Cog {}", entry.name))
        } else {
            None
        }
    }

    fn scan_for_cog_method(&mut self, pc: u64) -> Option<String> {
        let (mut start, end) = self.scan_bounds(pc)?;
        start &= !0x7;

        let mut candidate = pc & !0x7;
        while candidate >= start {
            if let Some(range) = self.read_cog_method(candidate, pc) {
                let name = range.name.clone();
                self.insert_method_range(range);
                return Some(name);
            }

            if candidate < 8 {
                break;
            }
            candidate -= 8;

            if candidate > end {
                break;
            }
        }

        None
    }

    fn scan_bounds(&self, pc: u64) -> Option<(u64, u64)> {
        if let Some(map) = self.maps.iter().find(|map| {
            let start = map.start() as u64;
            pc >= start && pc < start + map.size() as u64
        }) {
            let map_start = map.start() as u64;
            let map_end = map_start + map.size() as u64;
            let scan_start = pc.saturating_sub(COG_SCAN_LIMIT).max(map_start);
            return Some((scan_start, map_end));
        }

        self.method_zone_bounds
            .map(|(base, free_start)| (pc.saturating_sub(COG_SCAN_LIMIT).max(base), free_start))
    }

    fn read_cog_method(&self, addr: u64, pc: u64) -> Option<CogMethodRange> {
        let range = self.read_cog_method_at(addr)?;
        if pc < range.start + COG_METHOD_SIZE || pc >= range.end {
            return None;
        }
        Some(range)
    }

    fn read_cog_method_at(&self, addr: u64) -> Option<CogMethodRange> {
        let block_size: u16 = self
            .process
            .copy_struct((addr + COG_METHOD_BLOCK_SIZE_OFFSET) as usize)
            .ok()?;
        let block_size = u64::from(block_size);
        if !(COG_METHOD_SIZE..=COG_SCAN_LIMIT).contains(&block_size) {
            return None;
        }

        let end = round_up_to_method_alignment(addr.checked_add(block_size)?)?;

        let flags: u32 = self
            .process
            .copy_struct((addr + COG_METHOD_FLAGS_OFFSET) as usize)
            .ok()?;
        let cm_type = ((flags >> 8) & 0x7) as u8;

        let method_object = self
            .process
            .copy_struct((addr + COG_METHOD_METHOD_OBJECT_OFFSET) as usize)
            .ok()?;
        let method_header = self
            .process
            .copy_struct((addr + COG_METHOD_METHOD_HEADER_OFFSET) as usize)
            .ok()?;
        let selector_oop = self
            .process
            .copy_struct((addr + COG_METHOD_SELECTOR_OFFSET) as usize)
            .ok()?;

        let selector = self
            .read_selector(selector_oop)
            .or_else(|| {
                if matches!(cm_type, CM_METHOD | CM_METHOD_FLAGGED_FOR_BECOME) {
                    self.maybe_selector_of_method(method_object, method_header, 0)
                } else {
                    None
                }
            })
            .filter(|selector| looks_like_selector(selector));

        let name = if let Some(selector) = selector {
            match cm_type {
                CM_OPEN_PIC => format!("JIT PIC>>{selector}"),
                CM_CLOSED_PIC => format!("JIT closed PIC>>{selector}"),
                CM_METHOD | CM_METHOD_FLAGGED_FOR_BECOME => self
                    .class_name_of_method(method_object, method_header, 0)
                    .map(|class_name| format!("{class_name}>>{selector}"))
                    .unwrap_or_else(|| format!("JIT method>>{selector}")),
                _ => format!("JIT frame>>{selector}"),
            }
        } else {
            "JIT frame".to_owned()
        };

        Some(CogMethodRange {
            start: addr,
            end,
            name,
        })
    }

    fn refresh_method_zone(&mut self) {
        let Some(symbols) = self.method_zone_symbols else {
            return;
        };

        let Some((base, free_start)) = self.read_method_zone_bounds(symbols) else {
            debug!("OpenSmalltalk Cog method-zone bounds are not readable yet");
            return;
        };

        let previous_bounds = self.method_zone_bounds;
        let scan_start = if previous_bounds == Some((base, free_start)) {
            let cursor = self.method_zone_scan_cursor.unwrap_or(base);
            if cursor >= free_start || self.method_zone_blocked_cursor == Some(cursor) {
                return;
            }
            cursor
        } else {
            match previous_bounds {
                Some((previous_base, previous_free_start))
                    if previous_base == base && previous_free_start < free_start =>
                {
                    let cursor = self.method_zone_scan_cursor.unwrap_or(previous_free_start);
                    if self.method_zone_blocked_cursor == Some(cursor) {
                        self.method_zone_bounds = Some((base, free_start));
                        self.refresh_jit_entry_symbols();
                        return;
                    }
                    cursor
                }
                _ => {
                    self.method_ranges.clear();
                    self.resolved_pcs.clear();
                    self.method_zone_blocked_cursor = None;
                    base
                }
            }
        };

        if scan_start >= free_start {
            return;
        }

        self.method_zone_bounds = Some((base, free_start));
        self.refresh_jit_entry_symbols();

        let mut cursor = scan_start;
        while cursor < free_start {
            let Some(range) = self.read_cog_method_at(cursor) else {
                self.method_zone_scan_cursor = Some(cursor);
                self.method_zone_blocked_cursor = Some(cursor);
                debug!(
                    "OpenSmalltalk Cog method-zone walk stopped at 0x{:x}; bounds 0x{:x}..0x{:x}; ranges {}",
                    cursor,
                    base,
                    free_start,
                    self.method_ranges.len()
                );
                break;
            };

            if range.end <= cursor || range.end > free_start.saturating_add(8) {
                self.method_zone_scan_cursor = Some(cursor);
                self.method_zone_blocked_cursor = Some(cursor);
                break;
            }

            cursor = range.end;
            self.method_zone_blocked_cursor = None;
            self.insert_method_range(range);
        }
        self.method_zone_scan_cursor = Some(cursor);
        debug!(
            "OpenSmalltalk Cog method-zone bounds 0x{:x}..0x{:x}; loaded {} ranges",
            base,
            free_start,
            self.method_ranges.len()
        );
    }

    fn refresh_jit_entry_symbols(&mut self) {
        let mut entries: Vec<JitEntrySymbol> = self
            .jit_entry_symbol_slots
            .iter()
            .filter_map(|slot| {
                let address: u64 = self.process.copy_struct(slot.slot_addr as usize).ok()?;
                if address == 0 || !self.pc_looks_like_cog_code(address) {
                    return None;
                }
                Some(JitEntrySymbol {
                    address,
                    name: slot.name.clone(),
                })
            })
            .collect();

        entries.sort_by_key(|entry| entry.address);
        entries.dedup_by_key(|entry| entry.address);
        self.jit_entry_symbols = entries;
    }

    fn read_method_zone_bounds(&self, symbols: MethodZoneSymbols) -> Option<(u64, u64)> {
        let base: u64 = self
            .process
            .copy_struct(symbols.base_address_addr as usize)
            .ok()?;
        let free_start: u64 = self
            .process
            .copy_struct(symbols.free_start_addr as usize)
            .ok()?;

        if base == 0 || free_start <= base || free_start - base > MAX_METHOD_ZONE_SIZE {
            return None;
        }

        Some((base, free_start))
    }

    fn read_selector(&self, oop: u64) -> Option<String> {
        if oop == NIL_OBJECT {
            return None;
        }
        self.read_byte_object(oop)
    }

    fn maybe_selector_of_method(
        &self,
        method_obj: u64,
        method_header: u64,
        depth: u8,
    ) -> Option<String> {
        if depth > 4 || !self.is_compiled_method(method_obj) {
            return None;
        }

        let literal_count = self.literal_count_of_method(method_obj, method_header)?;
        if literal_count < 2 {
            return None;
        }

        let ultimate_literal = self.literal_of_method(method_obj, literal_count - 1)?;
        if self.is_compiled_method(ultimate_literal) {
            let nested_header = self.method_header_of(ultimate_literal)?;
            if let Some(selector) =
                self.maybe_selector_of_method(ultimate_literal, nested_header, depth + 1)
            {
                return Some(selector);
            }
        }

        let penultimate_literal = self.literal_of_method(method_obj, literal_count - 2)?;
        if self.is_words_or_bytes(penultimate_literal) {
            return self.read_selector(penultimate_literal);
        }

        if self.is_pointer_object(penultimate_literal) && self.num_slots(penultimate_literal)? >= 2
        {
            let method_field = self.literal_field(penultimate_literal, 0)?;
            let maybe_selector = self.literal_field(penultimate_literal, 1)?;
            if method_field == method_obj && self.is_words_or_bytes(maybe_selector) {
                return self.read_selector(maybe_selector);
            }
        }

        None
    }

    fn class_name_of_method(
        &self,
        method_obj: u64,
        method_header: u64,
        depth: u8,
    ) -> Option<String> {
        if depth > MAX_CLASS_NAME_DEPTH || !self.is_compiled_method(method_obj) {
            return None;
        }

        let literal_count = self.literal_count_of_method(method_obj, method_header)?;
        if literal_count == 0 {
            return None;
        }

        let first_literal = literal_count.saturating_sub(MAX_CLASS_LITERAL_SCAN);
        for offset in (first_literal..literal_count).rev() {
            let Some(literal) = self.literal_of_method(method_obj, offset) else {
                continue;
            };
            if self.is_compiled_method(literal) {
                if let Some(class_name) = self.method_header_of(literal).and_then(|nested_header| {
                    self.class_name_of_method(literal, nested_header, depth + 1)
                }) {
                    return Some(class_name);
                }
            }

            if let Some(class_name) = self.class_name_from_method_literal(literal, depth + 1) {
                return Some(class_name);
            }
        }

        None
    }

    fn class_name_from_method_literal(&self, oop: u64, depth: u8) -> Option<String> {
        if depth > MAX_CLASS_NAME_DEPTH || oop == NIL_OBJECT || !is_heap_object(oop) {
            return None;
        }

        if self.is_words_or_bytes(oop) {
            return self
                .read_byte_object(oop)
                .filter(|name| looks_like_class_name(name));
        }

        if !self.is_pointer_object(oop) {
            return None;
        }

        if let Some(class_name) = self.class_name_from_class_object(oop, depth + 1) {
            return Some(class_name);
        }

        let slots = self.num_slots(oop)?;
        if slots >= 2 {
            if let Some(value) = self.literal_field(oop, 1) {
                if let Some(class_name) = self.class_name_from_class_object(value, depth + 1) {
                    return Some(class_name);
                }
            }
            if let Some(key) = self.literal_field(oop, 0) {
                if let Some(class_name) = self
                    .read_byte_object(key)
                    .filter(|name| looks_like_class_name(name))
                {
                    return Some(class_name);
                }
            }
        }

        for offset in 0..slots.min(MAX_CLASS_OBJECT_SCAN) {
            if let Some(field) = self.literal_field(oop, offset) {
                if let Some(class_name) = self.class_name_from_method_literal(field, depth + 1) {
                    return Some(class_name);
                }
            }
        }

        None
    }

    fn class_name_from_class_object(&self, oop: u64, depth: u8) -> Option<String> {
        if depth > MAX_CLASS_NAME_DEPTH || !self.is_pointer_object(oop) {
            return None;
        }

        let slots = self.num_slots(oop)?;
        if slots > 6 {
            if let Some(name) = self.literal_field(oop, 6) {
                if let Some(class_name) = self
                    .read_byte_object(name)
                    .filter(|name| looks_like_class_name(name))
                {
                    return Some(class_name);
                }
            }
        }

        if slots > 5 {
            if let Some(this_class) = self.literal_field(oop, 5) {
                if this_class != oop {
                    if let Some(class_name) =
                        self.class_name_from_class_object(this_class, depth + 1)
                    {
                        return Some(format!("{class_name} class"));
                    }
                }
            }
        }

        for offset in 0..slots.min(MAX_CLASS_OBJECT_SCAN) {
            if matches!(offset, 5 | 6) {
                continue;
            }
            if let Some(name) = self.literal_field(oop, offset) {
                if let Some(class_name) = self
                    .read_byte_object(name)
                    .filter(|name| looks_like_class_name(name))
                {
                    return Some(class_name);
                }
            }
        }

        None
    }

    fn method_header_of(&self, method_obj: u64) -> Option<u64> {
        let header = self.literal_field(method_obj, HEADER_INDEX)?;
        if is_small_integer(header) {
            Some(header)
        } else {
            self.process
                .copy_struct((header + COG_METHOD_METHOD_HEADER_OFFSET) as usize)
                .ok()
        }
    }

    fn literal_count_of_method(&self, method_obj: u64, method_header: u64) -> Option<u64> {
        let header = if is_small_integer(method_header) {
            method_header
        } else {
            self.method_header_of(method_obj)?
        };
        Some((header >> 3) & ALTERNATE_HEADER_NUM_LITERALS_MASK)
    }

    fn literal_of_method(&self, method_obj: u64, offset: u64) -> Option<u64> {
        self.literal_field(method_obj, offset + LITERAL_START)
    }

    fn literal_field(&self, oop: u64, offset: u64) -> Option<u64> {
        self.process
            .copy_struct((oop + BASE_HEADER_SIZE + offset * WORD_SIZE as u64) as usize)
            .ok()
    }

    fn is_compiled_method(&self, oop: u64) -> bool {
        self.format_of(oop)
            .is_some_and(|format| format >= FIRST_COMPILED_METHOD_FORMAT)
    }

    fn is_words_or_bytes(&self, oop: u64) -> bool {
        self.format_of(oop).is_some_and(|format| {
            (FIRST_WORD_OR_BYTE_FORMAT..FIRST_COMPILED_METHOD_FORMAT).contains(&format)
        })
    }

    fn is_pointer_object(&self, oop: u64) -> bool {
        self.format_of(oop)
            .is_some_and(|format| format <= LAST_POINTER_FORMAT)
    }

    fn format_of(&self, oop: u64) -> Option<u8> {
        if !is_heap_object(oop) {
            return None;
        }
        self.process
            .copy_struct::<u8>((oop + FORMAT_FIELD_BYTE_OFFSET) as usize)
            .ok()
            .map(|byte| byte & FORMAT_MASK)
    }

    fn read_byte_object(&self, oop: u64) -> Option<String> {
        if !is_heap_object(oop) {
            return None;
        }

        let header = self
            .process
            .copy(oop as usize, BASE_HEADER_SIZE as usize)
            .ok()?;
        if header.len() != BASE_HEADER_SIZE as usize {
            return None;
        }

        let format = header[FORMAT_FIELD_BYTE_OFFSET as usize] & FORMAT_MASK;
        if !(FIRST_BYTE_FORMAT..FIRST_COMPILED_METHOD_FORMAT).contains(&format) {
            return None;
        }

        let slots = self.num_slots(oop)?;

        let byte_size = slots
            .checked_mul(WORD_SIZE as u64)?
            .checked_sub(u64::from(format & 7))?;
        if byte_size == 0 || byte_size > 512 {
            return None;
        }

        let bytes = self
            .process
            .copy((oop + BASE_HEADER_SIZE) as usize, byte_size as usize)
            .ok()?;
        if bytes
            .iter()
            .any(|byte| !byte.is_ascii_graphic() && *byte != b' ')
        {
            return None;
        }

        String::from_utf8(bytes).ok()
    }

    fn num_slots(&self, oop: u64) -> Option<u64> {
        if !is_heap_object(oop) {
            return None;
        }

        let slots: u8 = self
            .process
            .copy_struct((oop + NUM_SLOTS_FIELD_BYTE_OFFSET) as usize)
            .ok()?;
        if slots == NUM_SLOTS_MASK {
            let long_header: u64 = self
                .process
                .copy_struct((oop - BASE_HEADER_SIZE) as usize)
                .ok()?;
            Some(long_header >> 8)
        } else {
            Some(u64::from(slots))
        }
    }
}

fn is_heap_object(oop: u64) -> bool {
    oop != 0 && oop & TAG_MASK == 0
}

fn is_small_integer(oop: u64) -> bool {
    oop & TAG_MASK == SMALL_INTEGER_TAG
}

fn round_up_to_method_alignment(addr: u64) -> Option<u64> {
    addr.checked_add(7).map(|addr| addr & !0x7)
}

fn looks_like_selector(selector: &str) -> bool {
    !selector.is_empty()
        && selector.len() <= 256
        && selector
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+-*/\\~<>=@,%|&?!_:".contains(&byte))
}

fn looks_like_class_name(name: &str) -> bool {
    let Some(first) = name.as_bytes().first() else {
        return false;
    };
    first.is_ascii_uppercase()
        && name.len() <= 160
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn symbol_is_in_bss(binary: &BinaryInfo, addr: u64) -> bool {
    binary.bss_size != 0
        && addr >= binary.bss_addr
        && addr < binary.bss_addr.saturating_add(binary.bss_size)
}

fn looks_like_jit_entry_symbol(name: &str) -> bool {
    name.contains("Trampoline")
        || name.ends_with("Label")
        || name.ends_with("Entry")
        || name.ends_with("PIC")
        || name.starts_with("ce")
        || name.starts_with("blockEntry")
        || name == "stackCheckLabel"
        || name == "checkedEntryAlignment"
        || name == "uncheckedEntryAlignment"
        || name == "cPICPrototype"
}

fn clean_jit_entry_symbol_name(name: &str) -> String {
    name.strip_suffix("Trampoline").unwrap_or(name).to_owned()
}
