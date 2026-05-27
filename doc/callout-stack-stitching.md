# Callout Stack Stitching

When the Squeak VM calls a C primitive or FFI function, the Smalltalk call
chain that led to the callout becomes invisible to a native stack unwinder.

The Cog JIT compiles Smalltalk methods into native machine code.  When those
methods are executing, their frames sit on the OS stack and libunwind can
walk them normally. St-spy resolves the instruction pointers via the method
zone and everything works.

But when the VM enters a primitive or FFI callout, control transfers
from JIT-compiled code into the C interpreter loop and then into the
primitive's C implementation.  At this transition the VM saves the current
Smalltalk frame pointer into a global variable (`framePointer`) and sets up
a new C stack frame. The native stack looks like:

```
poll / recv / rlVertex3f   (libc / plugin)
DrawCube                   (libsqueakxrnative.so)
primitiveCallout           (X64SysVFFIPlugin.c)
Cog cePrimReturnEnterCogCode
                           *** gap -- no Smalltalk callers ***
```

The Smalltalk methods that called the primitive (`SWAGameXR>>renderOn:`,
`SRWorld>>render:`, etc.) are not on the OS stack.  They exist only in the
Cog internal frame chain, a linked list of frames in VM-managed stack page
memory, anchored by the `framePointer` global.

A pure native unwinder like libunwind cannot see these frames.  The result
is a disconnected flamegraph: wide islands of C code at the top with no
shared Smalltalk root.

## Cog Frame Layout (x86-64, 64-bit Spur)

Each Cog stack frame is laid out relative to a frame pointer (FP):

```
FP[0]    saved caller FP (0 = bottom of stack page)
FP[-8]   method field
FP[-16]  context / flags
FP[-24]  receiver (machine-code frames)
FP[-40]  receiver (interpreter frames)
```

The method field at `FP[-8]` distinguishes two frame types:

- **JIT frame** (`method < heapBase`): the value is a pointer to a
  `CogMethod` header in the method zone.  The method name can be resolved
  the same way st-spy already resolves JIT PCs, which reads the selector
  and class from the CogMethod's `methodObject`.

- **Interpreted frame** (`method >= heapBase`): the value is a Smalltalk
  context OOP.  The context's slot 3 (`oop + 8 + 3*8 = oop + 32`) holds the
  CompiledMethod, from which the selector and class can be read via the
  literal frame.

Walking the chain is straightforward: read `FP[0]` to get the caller's FP,
repeat until zero (bottom of stack page).

## VM Globals

Two BSS symbols in the Squeak binary provide the entry points:

| Symbol          | Purpose                                         |
|-----------------|-------------------------------------------------|
| `framePointer`  | Current Cog frame pointer (set before entering C) |
| `heapBase`      | Start of the Spur object heap (discriminator)    |

These are found by name in the ELF symbol table, like st-spy already finds `baseAddress` and
`mzFreeStart`.  The symbolizer reads their values from the target process via `copy_struct`.

## Stitching together the Stacks

### 1. Walk the Cog frame chain (`SmalltalkSymbolizer::walk_cog_frames`)

Added to `smalltalk_symbolizer.rs`.  Reads `framePointer` and `heapBase`
from the VM process, then follows the frame chain:

```
current_fp = read(framePointer)
while current_fp != 0:
    method_field = read(current_fp - 8)
    if method_field < heapBase:
        resolve as CogMethod (JIT frame)
    else:
        read context slot 3 -> CompiledMethod -> selector + class
    current_fp = read(current_fp)     # follow caller FP
```

A limit of 200 frames prevents runaway reads.  Each resolved method name is
returned in caller order (innermost first).

### 2. Detect the boundary (`SmalltalkSpy::find_interpreter_boundary`)

Added to `smalltalk_spy.rs`.  After the existing pass that resolves JIT PCs
in native frames, a second pass scans for the transition point -- frames
like:

- `Cog cePrimReturnEnterCogCode`, `Cog ceBaseFrameReturn` (Cog trampolines)
- `primitiveCallout`, `primitive*` (VM primitives)
- `interpret`, `ceSend*`, `ceReturn*` (interpreter runtime)

If the native stack has fewer than 4 real Smalltalk method frames (i.e. the
JIT unwind didn't capture a deep chain), the Cog frame walk is triggered and
the resulting Smalltalk frames are spliced in right after the boundary.

Deduplication ensures that if the innermost Cog frame was already resolved
by the native pass, it isn't inserted twice.

### 3. Heuristic: when NOT to splice

If the native unwind already captured more than 3 Smalltalk method frames
(not counting `Cog *` trampolines or `JIT *` PICs), the chain is
considered complete and no splicing occurs.  This avoids doubling frames
when the VM happens to be executing JIT code (not inside a callout) at
sample time.

## Result

Before:

```
rlVertex3f (libsqueakxrnative.so)
DrawCube (libsqueakxrnative.so)
primitiveCallout (X64SysVFFIPlugin.c)
Cog cePrimReturnEnterCogCode (Smalltalk)
```

After:

```
rlVertex3f (libsqueakxrnative.so)
DrawCube (libsqueakxrnative.so)
primitiveCallout (X64SysVFFIPlugin.c)
Cog cePrimReturnEnterCogCode (Smalltalk)
SWAGameXR>>renderOn: (Smalltalk)          <-- stitched
SRWorld>>render: (Smalltalk)              <-- stitched
SRFrame>>renderObjects (Smalltalk)        <-- stitched
BlockClosure>>on:do: (Smalltalk)          <-- stitched
SRFrame>>process (Smalltalk)              <-- stitched
SRWorld>>doOneCycle (Smalltalk)           <-- stitched
SRWorld>>loop (Smalltalk)                 <-- stitched
BlockClosure>>on:do: (Smalltalk)          <-- stitched
```

The flamegraph now shows a unified tree with native rendering code rooted
in the Smalltalk call chain, instead of disconnected islands.

## Files Changed

- `src/smalltalk_symbolizer.rs` -- `CogFrameSymbols` struct, `walk_cog_frames()`,
  `resolve_frame_method()`, frame chain constants
- `src/smalltalk_spy.rs` -- `find_interpreter_boundary()`, splice logic in
  `get_stack_traces()`

## Limitations

- The `framePointer` global reflects the state of whichever Smalltalk
  process was active when the VM entered C code.  If multiple Smalltalk
  processes are multiplexed on the same OS thread (which they always are in
  Squeak), we only see the active one.

- Interpreted frames require reading the context OOP and traversing the
  CompiledMethod literal frame to find the selector.  If the context or
  method is in an inconsistent state (e.g. mid-GC), the frame is silently
  skipped.

- The "> 3 Smalltalk frames" heuristic for skipping the splice is
  conservative.  In rare cases where the JIT unwind captures exactly the
  boundary frames plus a few trampolines, it might skip a useful splice.
  This can be tuned.

- Stack pages: when the Cog frame chain crosses a stack page boundary (the
  caller FP jumps to a different memory region), the walk continues as long
  as the reads succeed.  We do not currently validate that the target
  address is on a valid stack page.
