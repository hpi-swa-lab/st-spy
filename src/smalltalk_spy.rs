use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Error, Result};
use remoteprocess::{Pid, Process, Tid};

use crate::config::{Config, LockingStrategy};
use crate::native_stack_trace::NativeStack;
use crate::smalltalk_process_info::SmalltalkProcessInfo;
use crate::smalltalk_symbolizer::SmalltalkSymbolizer;
use crate::stack_trace::{Frame, StackTrace};

pub struct SmalltalkSpy {
    pub pid: Pid,
    pub process: Process,
    pub vm_version: String,
    pub config: Config,
    native: NativeStack,
    smalltalk_symbolizer: SmalltalkSymbolizer,
    short_filenames: HashMap<String, Option<String>>,
}

impl SmalltalkSpy {
    pub fn new(pid: Pid, config: &Config) -> Result<SmalltalkSpy, Error> {
        let process = remoteprocess::Process::new(pid)
            .context("Failed to open process - check if it is running.")?;

        let vm_info = SmalltalkProcessInfo::new(&process)?;
        info!("OpenSmalltalk VM detected: {}", vm_info.vm_version);

        let smalltalk_symbolizer = SmalltalkSymbolizer::new(pid, &process, vm_info.binary.as_ref());
        let native = NativeStack::new(pid)?;

        Ok(SmalltalkSpy {
            pid,
            process,
            vm_version: vm_info.vm_version,
            config: config.clone(),
            native,
            smalltalk_symbolizer,
            short_filenames: HashMap::new(),
        })
    }

    pub fn retry_new(pid: Pid, config: &Config, max_retries: u64) -> Result<SmalltalkSpy, Error> {
        let mut retries = 0;
        loop {
            let err = match SmalltalkSpy::new(pid, config) {
                Ok(mut process) => match process.get_stack_traces() {
                    Ok(_) => return Ok(process),
                    Err(err) => err,
                },
                Err(err) => err,
            };

            retries += 1;
            if retries >= max_retries {
                return Err(err);
            }
            info!("Failed to connect to process, retrying. Error: {}", err);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    pub fn get_stack_traces(&mut self) -> Result<Vec<StackTrace>, Error> {
        let mut thread_activity = HashMap::new();
        for thread in self.process.threads()?.iter() {
            let threadid: Tid = thread.id()?;
            let Ok(active) = thread.active() else {
                continue;
            };
            thread_activity.insert(threadid, active);
        }

        let _lock = if self.config.blocking == LockingStrategy::Lock {
            Some(self.process.lock().context("Failed to suspend process")?)
        } else {
            None
        };

        let mut traces = Vec::new();
        for thread in self.process.threads()?.iter() {
            let thread_id = match thread.id() {
                Ok(id) => id,
                Err(_) => continue,
            };
            let mut frames = self
                .native
                .thread_frames(thread)
                .with_context(|| format!("Failed to unwind thread {thread_id}"))?;

            // Pass 1: Resolve JIT code addresses to Smalltalk method names.
            for frame in &mut frames {
                if frame.filename == "?" {
                    if let Some(addr) = parse_hex_address(&frame.name) {
                        if let Some(name) = self.smalltalk_symbolizer.resolve_jit_pc(addr) {
                            frame.name = name;
                            frame.filename = "Smalltalk".to_owned();
                            frame.module = Some("Squeak".to_owned());
                        }
                    }
                }
                frame.short_filename = self.shorten_filename(&frame.filename);
            }

            // Pass 2: When the native stack shows we're inside a VM
            // primitive/FFI callout, the Smalltalk caller frames are NOT on the
            // native stack — they live in the Cog internal frame chain.  Detect
            // this boundary and splice in the Cog frames.
            if let Some(splice_pos) = Self::find_interpreter_boundary(&frames) {
                let cog_frames = self.smalltalk_symbolizer.walk_cog_frames();
                if !cog_frames.is_empty() {
                    // Deduplicate: if the innermost Cog frame is already on the
                    // native stack (resolved as Smalltalk), skip it.
                    let skip = if let Some(first_cog) = cog_frames.first() {
                        frames[..splice_pos]
                            .iter()
                            .any(|f| f.filename == "Smalltalk" && f.name == *first_cog)
                    } else {
                        false
                    };

                    let cog_frame_objects: Vec<Frame> = cog_frames
                        .into_iter()
                        .skip(if skip { 1 } else { 0 })
                        .map(|name| {
                            let short = self.shorten_filename("Smalltalk");
                            Frame {
                                name,
                                filename: "Smalltalk".to_owned(),
                                module: Some("Squeak".to_owned()),
                                short_filename: short,
                                line: 0,
                            }
                        })
                        .collect();

                    // Insert after the boundary frame so the trace reads:
                    //   ... native primitive frames ... | Cog Smalltalk frames ...
                    frames.splice(splice_pos..splice_pos, cog_frame_objects);
                }
            }

            traces.push(StackTrace {
                pid: self.pid,
                thread_id: thread_id as u64,
                thread_name: None,
                os_thread_id: Some(thread_id as u64),
                active: *thread_activity.get(&thread_id).unwrap_or(&true),
                frames,
                process_info: None,
            });
        }

        Ok(traces)
    }

    fn shorten_filename(&mut self, filename: &str) -> Option<String> {
        if self.config.full_filenames {
            return Some(filename.to_string());
        }

        if let Some(short) = self.short_filenames.get(filename) {
            return short.clone();
        }

        let shortened = Path::new(filename)
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.to_owned());
        self.short_filenames
            .insert(filename.to_owned(), shortened.clone());
        shortened
    }

    /// Look for the boundary where the VM transitions from Smalltalk execution
    /// into C code (primitives, FFI callouts, interpreter loop).
    ///
    /// Frames are ordered innermost-first.  We scan for known VM entry points
    /// that indicate "we're inside C code called from Smalltalk".  The splice
    /// point is just after the deepest such frame — that's where the Cog
    /// Smalltalk caller frames should be inserted.
    ///
    /// We look for patterns like:
    ///   - `primitiveCallout` / `primitive*` (FFI / VM primitives)
    ///   - `ceCaptureCStackPointers` (Cog→C transition)
    ///   - `interpret` (main interpreter loop)
    ///   - `ceSend*` / `ceReturn*` (Cog runtime helpers)
    ///   - Cog trampoline frames resolved as "Cog ce*"
    fn find_interpreter_boundary(frames: &[Frame]) -> Option<usize> {
        // The frames already contain some Smalltalk frames from JIT (resolved
        // in pass 1).  If we already have a deep Smalltalk call chain, there
        // is likely no disconnection to fix.  We only splice when we see a
        // Cog entry trampoline (like "Cog cePrimReturnEnterCogCode" or
        // "Cog ceBaseFrameReturn") near the bottom without corresponding
        // Smalltalk callers above it.

        // Find the position of the deepest Cog trampoline or VM primitive
        // frame — that's our boundary.  We want to insert Smalltalk frames
        // right after it (toward the caller/bottom of the stack).
        let mut boundary = None;
        let has_deep_smalltalk = frames
            .iter()
            .filter(|f| f.filename == "Smalltalk" && !f.name.starts_with("Cog ") && !f.name.starts_with("JIT "))
            .count()
            > 3;

        if has_deep_smalltalk {
            // The native unwind already captured a good Smalltalk stack.
            // Don't splice — it would likely duplicate frames.
            return None;
        }

        for (i, frame) in frames.iter().enumerate() {
            let dominated_by_cog_entry = frame.filename == "Smalltalk"
                && (frame.name.starts_with("Cog ce")
                    || frame.name.starts_with("Cog methodAbort"));

            let is_vm_primitive = frame.name.starts_with("primitive")
                && (frame.filename.ends_with("cointerp.c")
                    || frame.filename.ends_with("Plugin.c"));

            let is_interpret = frame.name == "interpret"
                || frame.name == "ceCaptureCStackPointers"
                || frame.name.starts_with("ceSend")
                || frame.name.starts_with("ceReturn");

            if dominated_by_cog_entry || is_vm_primitive || is_interpret {
                boundary = Some(i);
            }
        }

        // Return the position *after* the deepest boundary frame.
        boundary.map(|b| b + 1)
    }
}

fn parse_hex_address(value: &str) -> Option<u64> {
    value
        .strip_prefix("0x")
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
}
