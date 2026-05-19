# st-spy

`st-spy` is a sampling profiler for OpenSmalltalk VM programs. It can attach to
a running Squeak/OpenSmalltalk VM or launch one, unwind native VM stacks, resolve
Cog JIT frames back to Smalltalk selectors, and write flamegraph, speedscope,
raw folded-stack, or chrome trace output.

Note: `st-spy` is derived from `py-spy` and keeps the MIT license.

## Features

- `record`, `top`, and `dump` commands for profiling OpenSmalltalk VM processes.
- Native frame unwinding for VM, plugin, and libc frames.
- Cog method-zone decoding for class-qualified Smalltalk frames such as
  `Integer>>factorial` and `STSpyDeepNativeWorkload class>>deepNativeStack`.
- Cog trampoline and unresolved generated-code grouping as `Cog ...` or
  `JIT frame` instead of raw machine-code addresses where possible.
- Subprocess profiling for VM launchers that spawn the actual VM process.

## Build

```bash
cargo build --release
```

The binary is written to `target/release/st-spy`.

## Usage

Attach to a running VM:

```bash
st-spy top --pid 12345
st-spy dump --pid 12345
st-spy record --pid 12345 --output squeak.svg
```

Launch a VM and profile it:

```bash
st-spy record --duration 10 --rate 200 --output squeak.svg -- \
  /path/to/squeak.sh -headless -nosound /path/to/Squeak.image /path/to/workload.st
```

Useful options:

```text
-r, --rate <rate>          Samples per second
-i, --idle                 Include idle threads
-t, --threads              Include thread ids in record output
--full-filenames           Keep full source paths in native frames
-f, --format <format>      flamegraph, speedscope, raw, or chrometrace
```

On Linux, attaching to an already-running process is subject to the usual
`ptrace` restrictions. Launch-mode profiling is often the simplest way to test.

## Examples

The `examples/` directory contains Squeak scripts used to exercise mixed
Smalltalk and native stacks:

- `ffi-plugin-workload.st` calls safe Squeak FFI plugin primitives and keeps the
  image busy with integer work.
- `deep-native-plugin.c` and `deep-native-plugin-workload.st` build a scratch VM
  plugin that creates a deliberately deep native stack under a Smalltalk frame.

## License

MIT. See `LICENSE`.
