use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Error, Result};
use remoteprocess::{Pid, Process, Tid};

use crate::config::{Config, LockingStrategy};
use crate::native_stack_trace::NativeStack;
use crate::smalltalk_process_info::SmalltalkProcessInfo;
use crate::smalltalk_symbolizer::SmalltalkSymbolizer;
use crate::stack_trace::StackTrace;

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
}

fn parse_hex_address(value: &str) -> Option<u64> {
    value
        .strip_prefix("0x")
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
}
