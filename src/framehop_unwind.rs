//! Framehop-based native stack unwinder for Linux x86-64.
//!
//! This is the fast alternative to the libunwind backend in
//! `native_stack_trace.rs`.  libunwind reads target memory via a ptrace
//! accessor on *every* unwind-table and stack access, costing several
//! milliseconds per sample on a VM that links dozens of shared libraries.
//!
//! Framehop instead unwinds against:
//!   * pre-parsed, cached `.eh_frame` unwind info (loaded once per module), and
//!   * a *copy* of the thread's stack memory taken under the lock.
//!
//! Because of that, the only work that must happen while the VM is suspended
//! is: read the registers (one ptrace call) and copy a slice of the stack
//! (one `process_vm_readv`).  The expensive frame walk runs against the copy
//! and can therefore be moved off the lock entirely (Phase 2).
//!
//! Only compiled on Linux x86-64; every other target keeps using libunwind.

use anyhow::{anyhow, Context, Result};
use framehop::x86_64::{CacheX86_64, UnwindRegsX86_64, UnwinderX86_64};
use framehop::{ExplicitModuleSectionInfo, FrameAddress, Module, Unwinder};
use proc_maps::{get_process_maps, MapRange};
use remoteprocess::{Pid, ProcessMemory};

/// Number of bytes of stack we copy per sample, measured upward from rsp.
/// Stacks grow downward, so the live frames live in [rsp, rsp+window). The
/// full [stack] mapping is often 8 MB, but the *used* portion is typically a
/// few KB; copying the whole mapping (per thread, per sample) was the dominant
/// pause cost. 128 KB comfortably covers real call depths while keeping the
/// per-thread copy cheap. If a stack is genuinely deeper, framehop simply
/// stops unwinding when read_stack runs past the buffer (a truncated stack,
/// not a crash).
const STACK_COPY_WINDOW: usize = 128 * 1024;

/// A snapshot of everything needed to unwind one thread *without* touching the
/// target process again.  Captured under the lock; consumed (possibly off the
/// lock) by `Framehop::unwind_snapshot`.
pub struct StackSnapshot {
    pub rip: u64,
    pub regs: UnwindRegsX86_64,
    /// Copied stack bytes, starting at `stack_base` (= the captured rsp).
    pub stack: Vec<u8>,
    pub stack_base: u64,
}

pub struct Framehop {
    pid: Pid,
    unwinder: UnwinderX86_64<Vec<u8>>,
    cache: CacheX86_64,
    /// Module address ranges we've already added, to avoid re-adding on reload.
    loaded: Vec<std::ops::Range<u64>>,
    /// Cached writable mapping ranges (stacks live here). Used to bound the
    /// stack copy without re-reading /proc/<pid>/maps every sample. Refreshed
    /// on module reload.
    writable_ranges: Vec<std::ops::Range<u64>>,
}

impl Framehop {
    pub fn new(pid: Pid) -> Result<Framehop> {
        let mut fh = Framehop {
            pid,
            unwinder: UnwinderX86_64::new(),
            cache: CacheX86_64::new(),
            loaded: Vec::new(),
            writable_ranges: Vec::new(),
        };
        fh.load_modules()
            .context("failed to load modules for framehop unwinder")?;
        Ok(fh)
    }

    /// (Re)scan /proc/<pid>/maps and add any not-yet-known executable modules
    /// to the unwinder.  Safe to call repeatedly (e.g. after dlopen).
    pub fn load_modules(&mut self) -> Result<()> {
        let maps = get_process_maps(self.pid as proc_maps::Pid)
            .context("failed to read process maps")?;

        // Cache writable mapping ranges; thread stacks live in these. Lets
        // capture() bound the stack copy without re-reading maps per sample.
        self.writable_ranges = maps
            .iter()
            .filter(|m| m.is_write())
            .map(|m| (m.start() as u64)..((m.start() + m.size()) as u64))
            .collect();

        // Group executable mappings by backing file.  A single ELF can be
        // mapped as several adjacent segments; we need the file's load bias
        // (avma of the first segment minus its file offset region).
        let mut by_file: std::collections::HashMap<String, Vec<&MapRange>> =
            std::collections::HashMap::new();
        for m in &maps {
            let Some(path) = m.filename() else { continue };
            let Some(path) = path.to_str() else { continue };
            // Skip special/anonymous and non-file mappings.
            if path.is_empty() || path.starts_with('[') {
                continue;
            }
            by_file.entry(path.to_owned()).or_default().push(m);
        }

        for (path, segs) in by_file {
            // The avma range covered by all segments of this file.
            let avma_start = segs.iter().map(|s| s.start() as u64).min().unwrap();
            let avma_end = segs
                .iter()
                .map(|s| (s.start() + s.size()) as u64)
                .max()
                .unwrap();
            let range = avma_start..avma_end;

            // Already added? (cheap dedup by start address)
            if self.loaded.iter().any(|r| r.start == range.start) {
                continue;
            }

            match self.add_module_from_file(&path, &segs, range.clone()) {
                Ok(()) => self.loaded.push(range),
                Err(e) => {
                    // Non-fatal: a module without usable unwind info just means
                    // framehop falls back to frame-pointer unwinding there.
                    debug!("framehop: skipping module {}: {}", path, e);
                    // Still record it so we don't retry every reload.
                    self.loaded.push(range);
                }
            }
        }
        Ok(())
    }

    fn add_module_from_file(
        &mut self,
        path: &str,
        segs: &[&MapRange],
        avma_range: std::ops::Range<u64>,
    ) -> Result<()> {
        use goblin::elf::Elf;

        let data = std::fs::read(path).with_context(|| format!("reading {path}"))?;
        let elf = Elf::parse(&data).with_context(|| format!("parsing ELF {path}"))?;

        // Compute the module load bias: avma = svma + bias.
        // The bias is (mapping avma - mapping file offset) for the segment that
        // backs the ELF's lowest PT_LOAD vaddr.  In practice, for a PIE/.so the
        // first executable segment's (start - offset) gives the base avma that
        // corresponds to svma 0.
        let exec_seg = segs
            .iter()
            .find(|s| s.is_exec())
            .or_else(|| segs.first())
            .ok_or_else(|| anyhow!("no segments"))?;
        let base_avma = (exec_seg.start() as u64).wrapping_sub(exec_seg.offset as u64);

        // base_svma: the ELF's own notion of its base. For ET_DYN this is
        // typically 0; for ET_EXEC it's the lowest PT_LOAD p_vaddr.
        let base_svma = elf
            .program_headers
            .iter()
            .filter(|ph| ph.p_type == goblin::elf::program_header::PT_LOAD)
            .map(|ph| ph.p_vaddr)
            .min()
            .unwrap_or(0);

        // Extract section (svma range + bytes) by name.
        let section = |name: &str| -> Option<(std::ops::Range<u64>, Vec<u8>)> {
            for sh in &elf.section_headers {
                let sname = elf.shdr_strtab.get_at(sh.sh_name)?;
                if sname == name {
                    let start = sh.sh_addr;
                    let end = sh.sh_addr + sh.sh_size;
                    let bytes = if sh.sh_type == goblin::elf::section_header::SHT_NOBITS {
                        Vec::new()
                    } else {
                        let off = sh.sh_offset as usize;
                        let sz = sh.sh_size as usize;
                        data.get(off..off + sz)?.to_vec()
                    };
                    return Some((start..end, bytes));
                }
            }
            None
        };

        let text = section(".text");
        let eh_frame = section(".eh_frame");
        let eh_frame_hdr = section(".eh_frame_hdr");
        let got = section(".got");

        if eh_frame.is_none() && text.is_none() {
            return Err(anyhow!("no .eh_frame or .text"));
        }

        let info = ExplicitModuleSectionInfo {
            base_svma,
            text_svma: text.as_ref().map(|(r, _)| r.clone()),
            text: text.map(|(_, bytes)| bytes),
            eh_frame_svma: eh_frame.as_ref().map(|(r, _)| r.clone()),
            eh_frame: eh_frame.map(|(_, bytes)| bytes),
            eh_frame_hdr_svma: eh_frame_hdr.as_ref().map(|(r, _)| r.clone()),
            eh_frame_hdr: eh_frame_hdr.map(|(_, bytes)| bytes),
            got_svma: got.map(|(r, _)| r),
            ..Default::default()
        };

        let module = Module::new(path.to_owned(), avma_range, base_avma, info);
        self.unwinder.add_module(module);
        Ok(())
    }

    /// Capture registers + a copy of the stack for one thread.  Must be called
    /// while the target is suspended.  Cheap: one ptrace getregs + one memory
    /// copy.
    pub fn capture(&self, process: &remoteprocess::Process, tid: Pid) -> Result<StackSnapshot> {
        use nix::sys::ptrace;
        use nix::unistd::Pid as NixPid;

        let regs = ptrace::getregs(NixPid::from_raw(tid))
            .map_err(|e| anyhow!("ptrace getregs({tid}) failed: {e}"))?;

        let rip = regs.rip;
        let rsp = regs.rsp;
        let rbp = regs.rbp;

        // Copy a fixed window upward from rsp, clamped to the top of the
        // containing mapping so we never read past mapped memory. No per-sample
        // maps read (writable_ranges is cached).
        let window = STACK_COPY_WINDOW as u64;
        let top = match self.stack_top_for(rsp) {
            Some(map_top) => (rsp + window).min(map_top),
            None => rsp + window,
        };
        let len = top.saturating_sub(rsp) as usize;
        let stack = process
            .copy(rsp as usize, len)
            .map_err(|e| anyhow!("copying stack [{rsp:#x}, +{len}) failed: {e}"))?;

        Ok(StackSnapshot {
            rip,
            regs: UnwindRegsX86_64::new(rip, rsp, rbp),
            stack,
            stack_base: rsp,
        })
    }

    fn stack_top_for(&self, addr: u64) -> Option<u64> {
        self.writable_ranges
            .iter()
            .find(|r| addr >= r.start && addr < r.end)
            .map(|r| r.end)
    }

    /// Unwind a previously captured snapshot.  Does NOT touch the target
    /// process: reads come from the copied stack buffer.  Returns the list of
    /// instruction/return addresses, innermost first.
    pub fn unwind_snapshot(&mut self, snap: &StackSnapshot) -> Vec<u64> {
        let stack_base = snap.stack_base;
        let stack = &snap.stack;
        let mut read_stack = |addr: u64| -> std::result::Result<u64, ()> {
            if addr < stack_base {
                return Err(());
            }
            let off = (addr - stack_base) as usize;
            let end = off.checked_add(8).ok_or(())?;
            let bytes = stack.get(off..end).ok_or(())?;
            Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
        };

        let mut addrs = Vec::new();
        let mut iter =
            self.unwinder
                .iter_frames(snap.rip, snap.regs, &mut self.cache, &mut read_stack);
        while let Ok(Some(frame)) = iter.next() {
            let addr = match frame {
                FrameAddress::InstructionPointer(a) => a,
                FrameAddress::ReturnAddress(a) => a.into(),
            };
            addrs.push(addr);
        }
        addrs
    }

}
