use log::error;

// Simple example of showing how to use the rust API to
// print out sampled VM stack traces from an OpenSmalltalk process.

fn print_smalltalk_vm_stacks(pid: remoteprocess::Pid) -> Result<(), anyhow::Error> {
    let config = st_spy::Config::default();
    let mut process = st_spy::SmalltalkSpy::new(pid, &config)?;

    // get stack traces for each thread in the process
    let traces = process.get_stack_traces()?;

    // Print out the sampled VM stack for each thread
    for trace in traces {
        println!("Thread {:#X} ({})", trace.thread_id, trace.status_str());
        for frame in &trace.frames {
            println!("\t {} ({}:{})", frame.name, frame.filename, frame.line);
        }
    }
    Ok(())
}

fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    let pid = if args.len() > 1 {
        args[1].parse().expect("invalid pid")
    } else {
        error!("you must specify a pid!");
        return;
    };

    if let Err(e) = print_smalltalk_vm_stacks(pid) {
        error!("failed to print stack traces: {:?}", e);
    }
}
