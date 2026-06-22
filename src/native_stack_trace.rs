use std::num::NonZeroUsize;

use anyhow::Error;
use cpp_demangle::{BorrowedSymbol, DemangleOptions};
use lru::LruCache;
use remoteprocess::{self, Pid};

use crate::config::UnwinderKind;
use crate::stack_trace::Frame;
use crate::utils::resolve_filename;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use crate::framehop_unwind::Framehop;

/// The address-producing backend.  Both produce a `Vec<u64>` of return
/// addresses; symbolication afterwards is identical.
enum Backend {
    Libunwind(remoteprocess::Unwinder),
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    Framehop(Framehop),
}

/// Result of the under-lock capture phase. Libunwind must unwind while the
/// target is suspended, so it already holds resolved addresses. Framehop only
/// copied registers + stack and defers the actual walk to the off-lock resolve
/// phase.
pub enum ThreadCapture {
    Addresses(Vec<u64>),
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    Snapshot(crate::framehop_unwind::StackSnapshot),
}

pub struct NativeStack {
    should_reload: bool,
    backend: Backend,
    symbolicator: remoteprocess::Symbolicator,
    #[allow(dead_code)]
    pid: Pid,
    /// Captures since the last framehop module refresh. We refresh periodically
    /// (not per-sample) so newly dlopen'd libraries get unwind info without
    /// paying the maps-parse + ELF-read cost on the hot path.
    captures_since_reload: u32,
    // On Windows, unwinding needs the process handle to stay alive.
    process: remoteprocess::Process,
    symbol_cache: LruCache<u64, remoteprocess::StackFrame>,
}

impl NativeStack {
    pub fn new(pid: Pid, kind: UnwinderKind) -> Result<NativeStack, Error> {
        let process = remoteprocess::Process::new(pid)?;
        let symbolicator = process.symbolicator()?;

        let backend = match kind {
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            UnwinderKind::framehop => {
                info!("using framehop native unwinder");
                Backend::Framehop(Framehop::new(pid)?)
            }
            #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
            UnwinderKind::framehop => {
                info!("framehop unwinder unavailable on this platform; using libunwind");
                Backend::Libunwind(process.unwinder()?)
            }
            UnwinderKind::libunwind => {
                info!("using libunwind native unwinder");
                Backend::Libunwind(process.unwinder()?)
            }
        };

        Ok(NativeStack {
            backend,
            symbolicator,
            captures_since_reload: 0,
            should_reload: false,
            pid,
            process,
            symbol_cache: LruCache::new(NonZeroUsize::new(65536).unwrap()),
        })
    }

    /// Phase A (under lock): grab the minimum needed to unwind a thread later.
    /// For libunwind this still unwinds immediately (it has no off-lock mode);
    /// for framehop this only copies registers + stack, deferring the walk.
    pub fn capture(&mut self, thread: &remoteprocess::Thread) -> Result<ThreadCapture, Error> {
        // Periodically refresh framehop's module list so dlopen'd libraries get
        // unwind info. Cheap amortized: once every REFRESH captures, not each.
        // (Native symbolicator reload is deferred to resolve(), off the lock.)
        const FRAMEHOP_REFRESH_INTERVAL: u32 = 2000;
        self.captures_since_reload += 1;
        if self.captures_since_reload >= FRAMEHOP_REFRESH_INTERVAL {
            self.reload_backend();
            self.captures_since_reload = 0;
        }

        match &mut self.backend {
            Backend::Libunwind(unwinder) => {
                let mut addrs = Vec::new();
                for ip in unwinder.cursor(thread)? {
                    addrs.push(ip?);
                }
                Ok(ThreadCapture::Addresses(addrs))
            }
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Backend::Framehop(fh) => {
                let tid = thread.id()? as Pid;
                let snap = fh.capture(&self.process, tid)?;
                Ok(ThreadCapture::Snapshot(snap))
            }
        }
    }

    /// Phase B (off lock): turn a capture into symbolized frames. For framehop
    /// this performs the actual stack walk against the copied buffer; for both
    /// backends it symbolizes the resulting addresses. Touches only the disk
    /// ELF / symbol cache, never the live (now-resumed) target.
    pub fn resolve(&mut self, capture: ThreadCapture) -> Vec<Frame> {
        // Reload native symbol info if a previous sample saw an unknown address.
        // Done here (off the lock) rather than in capture(), since it only
        // affects symbolication and reads on-disk data, not the live target.
        if self.should_reload {
            if let Err(e) = self.symbolicator.reload() {
                debug!("symbolicator reload failed: {e}");
            }
            self.should_reload = false;
        }
        let addrs = match capture {
            ThreadCapture::Addresses(a) => a,
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            ThreadCapture::Snapshot(snap) => match &mut self.backend {
                Backend::Framehop(fh) => fh.unwind_snapshot(&snap),
                // Shouldn't happen: snapshots only come from framehop.
                Backend::Libunwind(_) => Vec::new(),
            },
        };
        self.symbolize_addresses(addrs)
    }

    fn symbolize_addresses(&mut self, native_stack: Vec<u64>) -> Vec<Frame> {
        let mut frames = Vec::new();
        for addr in native_stack {
            let cached_symbol = self.symbol_cache.get(&addr).cloned();
            if let Some(frame) = cached_symbol {
                if let Some(frame) = self.translate_native_frame(&frame) {
                    frames.push(frame);
                }
                continue;
            }

            let mut symbolicated_count = 0;
            let mut first_frame = None;
            self.symbolicator
                .symbolicate(addr, true, &mut |frame: &remoteprocess::StackFrame| {
                    symbolicated_count += 1;
                    if symbolicated_count == 1 {
                        first_frame = Some(frame.clone());
                    }
                    if let Some(frame) = self.translate_native_frame(frame) {
                        frames.push(frame);
                    }
                })
                .unwrap_or_else(|e| {
                    if let remoteprocess::Error::NoBinaryForAddress(_) = e {
                        debug!(
                            "don't have a binary for symbols at 0x{:x} - reloading",
                            addr
                        );
                        self.should_reload = true;
                    }
                    frames.push(Frame {
                        filename: "?".to_owned(),
                        name: format!("0x{:x}", addr),
                        line: 0,
                        short_filename: None,
                        module: None,
                    });
                });

            if symbolicated_count == 1 {
                self.symbol_cache.put(addr, first_frame.unwrap());
            }
        }

        frames
    }

    /// Translates a native frame into an optional frame. None indicates we should ignore it.
    fn translate_native_frame(&self, frame: &remoteprocess::StackFrame) -> Option<Frame> {
        match &frame.function {
            Some(func) => {
                if ignore_frame(func, &frame.module) {
                    return None;
                }

                let filename = match frame.filename.as_ref() {
                    Some(filename) => resolve_filename(filename, &frame.module)
                        .unwrap_or_else(|| filename.clone()),
                    None => frame.module.clone(),
                };

                let mut demangled = None;
                if func.starts_with('_') {
                    if let Ok((sym, _)) = BorrowedSymbol::with_tail(func.as_bytes()) {
                        let options = DemangleOptions::new().no_params().no_return_type();
                        if let Ok(sym) = sym.demangle_with_options(&options) {
                            demangled = Some(sym);
                        }
                    }
                }

                Some(Frame {
                    filename,
                    line: frame.line.unwrap_or(0) as i32,
                    name: demangled.as_ref().unwrap_or(func).to_owned(),
                    short_filename: None,
                    module: Some(frame.module.clone()),
                })
            }
            None => Some(Frame {
                filename: frame.module.clone(),
                name: format!("0x{:x}", frame.addr),
                line: 0,
                short_filename: None,
                module: Some(frame.module.clone()),
            }),
        }
    }

    /// Re-scan modules when the symbolicator reloaded (e.g. after dlopen) so
    /// framehop learns about newly mapped libraries.
    fn reload_backend(&mut self) {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        if let Backend::Framehop(fh) = &mut self.backend {
            if let Err(e) = fh.load_modules() {
                debug!("framehop module reload failed: {e}");
            }
        }
    }
}

// Remove the top-level runtime frames that do not add useful context.
#[cfg(target_os = "linux")]
fn ignore_frame(function: &str, module: &str) -> bool {
    if function == "__libc_start_main" && module.contains("/libc") {
        return true;
    }

    if function == "__clone" && module.contains("/libc") {
        return true;
    }

    if function == "start_thread" && module.contains("/libpthread") {
        return true;
    }

    false
}

#[cfg(target_os = "macos")]
fn ignore_frame(function: &str, module: &str) -> bool {
    if function == "_start" && module.contains("/libdyld.dylib") {
        return true;
    }

    if function == "__pthread_body" && module.contains("/libsystem_pthread") {
        return true;
    }

    if function == "_thread_start" && module.contains("/libsystem_pthread") {
        return true;
    }

    false
}

#[cfg(windows)]
fn ignore_frame(function: &str, module: &str) -> bool {
    if function == "RtlUserThreadStart" && module.to_lowercase().ends_with("ntdll.dll") {
        return true;
    }

    if function == "BaseThreadInitThunk" && module.to_lowercase().ends_with("kernel32.dll") {
        return true;
    }

    false
}
