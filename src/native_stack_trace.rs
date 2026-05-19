use std::num::NonZeroUsize;

use anyhow::Error;
use cpp_demangle::{BorrowedSymbol, DemangleOptions};
use lru::LruCache;
use remoteprocess::{self, Pid};

use crate::stack_trace::Frame;
use crate::utils::resolve_filename;

pub struct NativeStack {
    should_reload: bool,
    unwinder: remoteprocess::Unwinder,
    symbolicator: remoteprocess::Symbolicator,
    // On Windows, unwinding needs the process handle to stay alive.
    #[allow(dead_code)]
    process: remoteprocess::Process,
    symbol_cache: LruCache<u64, remoteprocess::StackFrame>,
}

impl NativeStack {
    pub fn new(pid: Pid) -> Result<NativeStack, Error> {
        let process = remoteprocess::Process::new(pid)?;
        let unwinder = process.unwinder()?;
        let symbolicator = process.symbolicator()?;

        Ok(NativeStack {
            unwinder,
            symbolicator,
            should_reload: false,
            process,
            symbol_cache: LruCache::new(NonZeroUsize::new(65536).unwrap()),
        })
    }

    pub fn thread_frames(&mut self, thread: &remoteprocess::Thread) -> Result<Vec<Frame>, Error> {
        if self.should_reload {
            self.symbolicator.reload()?;
            self.should_reload = false;
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

        Ok(frames)
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

    fn get_thread(&mut self, thread: &remoteprocess::Thread) -> Result<Vec<u64>, Error> {
        let mut stack = Vec::new();
        for ip in self.unwinder.cursor(thread)? {
            stack.push(ip?);
        }
        Ok(stack)
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
