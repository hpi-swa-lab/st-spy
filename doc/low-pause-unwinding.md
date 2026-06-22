# Low-pause native unwinding

st-spy is a sampling profiler, so it works by repeatedly stopping the target VM,
reading its stacks, and letting it run again. Every moment the VM is stopped is
overhead that slows down the very program we are trying to measure. This note
explains how we got that stop time down, and why the approach is safe.

When st-spy was forked from py-spy it inherited py-spy's Linux unwinder, which
is libunwind driven through the remoteprocess crate. libunwind is reliable and
works on many platforms, but it reads the target's memory through a ptrace
accessor, which means every lookup of unwind information and every read of stack
memory is its own syscall into the stopped process. A Squeak VM links dozens of
shared libraries (libX11, mesa, pulse, the GL stack, FFI plugins, and so on), so
a single unwind walks unwind tables across many of those modules and ends up
issuing thousands of tiny reads, all while the VM is frozen.

Measured on a real Squeak VM at 100 Hz, that gave a per-sample stop of roughly
4.3 ms: about 480 us to seize and interrupt the threads, about 3.65 ms for the
libunwind walk itself, a negligible 8 us for the Cog frame-chain walk, and about
160 us to detach and resume. The native walk is almost the whole cost; the
Smalltalk-specific work is noise. At 100 Hz a 4.3 ms stop means the VM is frozen
roughly 45% of the time.

## Unwinding against a copied stack

The first change was to replace libunwind with framehop
(https://github.com/mstange/framehop), a unwinder written in Rust and aimed at
sampling profilers; it is the same one samply uses. The important property is
that framehop unwinds from two things that don't require the target to stay
stopped: unwind information parsed and cached once per module, and a copy of the
thread's stack memory. So the only work that has to happen while the VM is
stopped is reading the thread's registers (one ptrace getregs) and copying a
slice of its stack (one process_vm_readv). The actual frame walk then runs over
the copy and makes no further syscalls into the target.

This lives in `src/framehop_unwind.rs`. `Framehop::load_modules` parses
`/proc/<pid>/maps` and, for each mapped ELF, pulls out the `.text`, `.eh_frame`,
`.eh_frame_hdr`, and `.got` sections with goblin, works out the load bias, and
registers a framehop module. `Framehop::capture` runs under the lock and does
the getregs plus a bounded (128 KB) copy of the stack upward from rsp, returning
a self-contained snapshot. `Framehop::unwind_snapshot` runs off the lock and
walks the frames against that copied buffer. Where there is no `.eh_frame` (the
Cog JIT code zone, for instance) framehop falls back to frame-pointer unwinding,
which works because Cog frames keep frame pointers.

## Doing the rest off the lock

Switching to framehop was not enough on its own. Profiling the stop showed that
two more things were still happening while the VM was frozen even though they did
not need to be. One was native symbol resolution, turning an address into a
function name and source location, which only reads on-disk ELF debug info and
an in-process cache. The other was the reload calls that fired whenever a sampled
address had no debug info, which is common for stripped system libraries: the
native symbolicator's reload (around 500 us) and framehop's module reload (around
1 ms) were both running on every single sample, inside the lock.

So `SmalltalkSpy::get_stack_traces` in `src/smalltalk_spy.rs` now splits a sample
into two phases. While the VM is stopped it only captures raw data: for each
thread it calls `NativeStack::capture` (the framehop registers and stack copy),
and once it snapshots the Cog frame chain with `walk_cog_frames`, which has to be
read live but costs about a microsecond. Then it detaches and the VM resumes.
Everything else happens afterward, with the VM already running: for each captured
thread it calls `NativeStack::resolve`, which does the framehop walk over the
frozen stack copy and then symbolizes the addresses, and the Cog-frame splice
described in `callout-stack-stitching.md` uses the snapshot taken in the first
phase. The native symbolicator reload is deferred into this second phase too,
and framehop's module list is refreshed only every couple of thousand captures
rather than every sample, so newly dlopen'd libraries still get unwind info
without paying for a maps parse and ELF read on the hot path.

This is correct because the data captured in the first phase is self-contained.
The stack copy freezes the bytes framehop needs, so the VM changing its stack
after it resumes cannot affect a walk over the copy. The Cog frame chain is
snapshotted under the lock, so it reflects the VM at the sample instant. Native
symbolization only reads files on disk and a cache, never the live process. The
one place we accept an approximation is JIT-PC-to-selector resolution
(`resolve_jit_pc`), which reads the live method zone during the second phase; it
is cached and the method zone only changes across the sub-millisecond gap between
capture and resolve, so a misattributed JIT frame is possible but very unlikely,
and that trade buys most of the pause reduction.

## Result

On the same VM at 100 Hz, the per-sample stop went from about 4.3 ms with
libunwind, to about 1.85 ms with framehop while still walking under the lock, to
about 790 us once the walk and symbolization moved off the lock. That is roughly
a 5.4x reduction in how long the VM is interrupted, with no loss of stack quality
(full native stacks plus the Smalltalk Cog splice). The duty cycle drops from
about 45% to about 8%. What is left is mostly the ptrace seize/interrupt and
detach round-trip across all the threads (around 660 us) plus the stack copy
(around 50 us), which is close to the irreducible cost of stopping and resuming
the process.

## Choosing the backend

libunwind is still there, both as a fallback and so the two can be compared. The
backend is selectable with `--unwinder framehop` or `--unwinder libunwind`.
framehop is the default on Linux x86-64; on any other platform, or when asked for
explicitly, st-spy uses libunwind. That keeps the profiler working everywhere
while making the fast path the default where it exists.

## Seeing the numbers

The per-sample stop breakdown is logged at info level, so it stays quiet
normally. Run with `RUST_LOG=info` and you'll see lines like:

```text
... N samples | PAUSE avg=790us (min=506 max=...) [lock=510 cog=52 detach=158]
    | off-lock walk+symbolize=650us | duty~8.0%
```

PAUSE is the time the VM is actually stopped (lock, capture, cog walk, detach).
The off-lock walk+symbolize figure is the work done after the VM resumed and is
not part of the stop. duty is pause times rate, the fraction of wall time the VM
is frozen.

## What changed in the code

The new module is `src/framehop_unwind.rs`, holding the module loader, the
register and stack capture, and the off-lock unwind (`Framehop` and
`StackSnapshot`). `src/native_stack_trace.rs` gained a selectable backend
(libunwind or framehop), the two-phase `capture` and `resolve` methods, the
deferred symbolicator reload, and the `ThreadCapture` type. `src/smalltalk_spy.rs`
holds the two-phase `get_stack_traces` and the pause-timing report.
`src/config.rs` adds the `UnwinderKind`, the `--unwinder` flag, and the
platform default. `Cargo.toml` pulls in framehop and nix (for ptrace getregs).

## Limitations and possible next steps

The stack copy is capped at 128 KB above rsp, so a genuinely deeper stack is
unwound only that far (a truncated stack, never a crash); the cap can be raised
if deep native call chains are expected. The remaining stop is dominated by the
ptrace round-trip across all threads, and shrinking it further would mean locking
fewer threads or doing fewer round-trips, which is a separate piece of work.
Finally, the off-lock symbolization currently runs on the sampling thread between
samples; if it ever grew long enough to exceed the gap between samples it could
move to its own worker thread without affecting the stop time.
