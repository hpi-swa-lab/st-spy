//! st-spy: a sampling profiler for OpenSmalltalk VM programs
//!
//! This crate lets you use st-spy as a rust library, and gather stack traces from
//! an OpenSmalltalk VM process programmatically.
//!
//! # Example:
//!
//! ```rust,no_run
//! fn print_smalltalk_vm_stacks(pid: st_spy::Pid) -> Result<(), anyhow::Error> {
//!     // Create a new SmalltalkSpy object with the default config options
//!     let config = st_spy::Config::default();
//!     let mut process = st_spy::SmalltalkSpy::new(pid, &config)?;
//!
//!     // get stack traces for each thread in the process
//!     let traces = process.get_stack_traces()?;
//!
//!     // Print out the sampled VM stack for each thread
//!     for trace in traces {
//!         println!("Thread {:#X} ({})", trace.thread_id, trace.status_str());
//!         for frame in &trace.frames {
//!             println!("\t {} ({}:{})", frame.name, frame.filename, frame.line);
//!         }
//!     }
//!     Ok(())
//! }
//! ```
#[macro_use]
extern crate anyhow;
#[macro_use]
extern crate log;

pub mod binary_parser;
pub mod config;
#[cfg(feature = "cli")]
pub mod dump;
#[cfg(feature = "unwind")]
mod native_stack_trace;
pub mod sampler;
pub mod smalltalk_process_info;
#[cfg(feature = "unwind")]
pub mod smalltalk_spy;
#[cfg(feature = "unwind")]
mod smalltalk_symbolizer;
pub mod stack_trace;
pub mod timer;
mod utils;

pub use config::Config;
pub use remoteprocess::Pid;
#[cfg(feature = "unwind")]
pub use smalltalk_spy::SmalltalkSpy;
pub use stack_trace::Frame;
pub use stack_trace::StackTrace;
