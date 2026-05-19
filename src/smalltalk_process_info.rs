use std::path::PathBuf;

use anyhow::{Context, Error, Result};
use proc_maps::get_process_maps;
use remoteprocess::ProcessMemory;

use crate::binary_parser::{parse_binary, BinaryInfo};

const VM_SYMBOLS: &[&str] = &[
    "interpreterVersion",
    "__interpBuildInfo",
    "interpret",
    "vmIsInitialized",
    "fullDisplayUpdate",
];

pub struct SmalltalkProcessInfo {
    pub binary: Option<BinaryInfo>,
    pub vm_version: String,
}

impl SmalltalkProcessInfo {
    pub fn new(process: &remoteprocess::Process) -> Result<SmalltalkProcessInfo, Error> {
        let executable = process.exe().context("Failed to find process executable")?;
        let executable_path = PathBuf::from(&executable);
        let executable_lower = executable.to_lowercase();

        let maps = get_process_maps(process.pid)?;
        let map = maps
            .iter()
            .filter(|m| m.is_exec())
            .find(|m| {
                m.filename()
                    .is_some_and(|filename| filename == executable_path.as_path())
            })
            .context("Failed to find executable memory map")?;

        #[cfg(target_os = "linux")]
        let binary_path = PathBuf::from(format!("/proc/{}/exe", process.pid));

        #[cfg(not(target_os = "linux"))]
        let binary_path = executable_path.clone();

        let binary = parse_binary(&binary_path, map.start() as u64, map.size() as u64).ok();
        let symbol_match = binary.as_ref().is_some_and(|binary| {
            VM_SYMBOLS
                .iter()
                .any(|symbol| binary.symbols.contains_key(*symbol))
        });
        let name_match = executable_lower.contains("squeak")
            || executable_lower.contains("opensmalltalk")
            || executable_lower.contains("spur")
            || executable_lower.contains("cog");

        if !symbol_match && !name_match {
            return Err(format_err!(
                "Target executable '{}' does not look like an OpenSmalltalk VM",
                executable
            ));
        }

        let vm_version = binary
            .as_ref()
            .and_then(|binary| read_vm_version(binary, process).ok())
            .unwrap_or_else(|| {
                executable_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("OpenSmalltalk VM")
                    .to_owned()
            });

        Ok(SmalltalkProcessInfo { binary, vm_version })
    }
}

fn read_vm_version<P: ProcessMemory>(binary: &BinaryInfo, process: &P) -> Result<String, Error> {
    for symbol in ["interpreterVersion", "__interpBuildInfo"] {
        if let Some(&addr) = binary.symbols.get(symbol) {
            if let Ok(ptr) = process.copy_struct::<usize>(addr as usize) {
                if ptr != 0 {
                    if let Ok(version) = copy_c_string(process, ptr) {
                        if is_vm_version(&version) {
                            return Ok(version);
                        }
                    }
                }
            }

            if let Ok(version) = copy_c_string(process, addr as usize) {
                if is_vm_version(&version) {
                    return Ok(version);
                }
            }
        }
    }

    Err(format_err!("Failed to read OpenSmalltalk VM version"))
}

fn copy_c_string<P: ProcessMemory>(process: &P, addr: usize) -> Result<String, Error> {
    let mut bytes = Vec::new();
    for offset in 0..4096 {
        let byte: u8 = process.copy_struct(addr + offset)?;
        if byte == 0 {
            break;
        }
        bytes.push(byte);
    }

    if bytes.is_empty() {
        return Err(format_err!("Empty C string"));
    }

    Ok(String::from_utf8(bytes)?)
}

fn is_vm_version(value: &str) -> bool {
    value.contains("Open Smalltalk")
        || value.contains("OpenSmalltalk")
        || value.contains("StackInterpreter")
        || value.contains("Cog")
}
