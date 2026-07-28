// Full native unwinding: Linux with libunwind, Windows.
#[cfg(any(unwind, windows))]
mod native {
    use std::num::NonZeroUsize;
    use std::time::{Duration, Instant};

    use anyhow::Error;
    use cpp_demangle::{BorrowedSymbol, DemangleOptions};
    use lru::LruCache;
    use remoteprocess::Pid;

    use crate::stack_trace::Frame;
    use crate::utils::resolve_filename;

    // Reloading re-scans every memory mapping of the target process, which
    // can be expensive on processes with a large, fragmented address space
    // (e.g. a GPU-heavy macOS app with hundreds of driver/framework
    // mappings). Multiple threads can each independently flag a reload
    // within the same sample, so without a cooldown this can fire far more
    // often than the sampling rate can tolerate. A newly-mapped binary just
    // shows raw addresses until the next reload is due.
    const RELOAD_COOLDOWN: Duration = Duration::from_millis(250);

    pub struct NativeStack {
        should_reload: bool,
        last_reload: Option<Instant>,
        line_info: bool,
        unwinder: remoteprocess::Unwinder,
        symbolicator: remoteprocess::Symbolicator,
        #[allow(dead_code)]
        process: remoteprocess::Process,
        symbol_cache: LruCache<u64, remoteprocess::StackFrame>,
    }

    impl NativeStack {
        pub fn new(pid: Pid, line_info: bool) -> Result<NativeStack, Error> {
            let process = remoteprocess::Process::new(pid)?;
            let unwinder = process.unwinder()?;
            let symbolicator = process.symbolicator()?;

            Ok(NativeStack {
                unwinder,
                symbolicator,
                should_reload: false,
                last_reload: None,
                line_info,
                process,
                symbol_cache: LruCache::new(NonZeroUsize::new(65536).unwrap()),
            })
        }

        pub fn thread_frames(
            &mut self,
            thread: &remoteprocess::Thread,
        ) -> Result<Vec<Frame>, Error> {
            if self.should_reload {
                let due = self
                    .last_reload
                    .is_none_or(|t| t.elapsed() >= RELOAD_COOLDOWN);
                if due {
                    self.symbolicator.reload()?;
                    self.last_reload = Some(Instant::now());
                    self.should_reload = false;
                }
            }

            let native_stack = self.get_thread(thread)?;
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
                    .symbolicate(addr, self.line_info, &mut |frame: &remoteprocess::StackFrame| {
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

            Ok(frames)
        }

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

        fn get_thread(&mut self, thread: &remoteprocess::Thread) -> Result<Vec<u64>, Error> {
            let mut stack = Vec::new();
            for ip in self.unwinder.cursor(thread)? {
                stack.push(ip?);
            }
            Ok(stack)
        }
    }

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
}

// Stub for platforms without native unwinding (macOS, Linux without libunwind).
#[cfg(not(any(unwind, windows)))]
mod native {
    use anyhow::Error;
    use remoteprocess::Pid;

    use crate::stack_trace::Frame;

    pub struct NativeStack;

    impl NativeStack {
        pub fn new(_pid: Pid, _line_info: bool) -> Result<NativeStack, Error> {
            Ok(NativeStack)
        }

        pub fn thread_frames(
            &mut self,
            _thread: &remoteprocess::Thread,
        ) -> Result<Vec<Frame>, Error> {
            Ok(vec![])
        }
    }
}

pub use native::NativeStack;
