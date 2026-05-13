## Preload interception library

Read `README.md` in this directory for platform details and build instructions.

## Architecture constraint

The library is split into C shim (`src/c/shim.c`) and Rust (`src/implementation.rs`).
This split is mandatory:

1. Stable Rust cannot handle C variadic arguments (`execl` family)
2. On FreeBSD, having all exported symbols in C avoids recursive interception via `dlsym(RTLD_NEXT, ...)`

Do not merge the C and Rust parts.

## Supported platforms

| Platform | Mechanism | Symbol visibility |
|---|---|---|
| Linux, FreeBSD, OpenBSD, NetBSD, DragonFly BSD | `LD_PRELOAD` | ELF version scripts |
| macOS | `DYLD_INSERT_LIBRARIES` | `-exported_symbols_list` |

The code uses `cfg(target_os = "macos")` vs `cfg(not(target_os = "macos"))`,
so all non-macOS Unix platforms use the `LD_PRELOAD` path.

On unsupported platforms (e.g., Windows), the build warns and skips library generation.

## What it intercepts

`exec` family, `posix_spawn`, `popen`, `system`. Only functions detected as
available on the host at build time (via `platform-checks`).

## Build script duties

`build.rs` is platform-gated to `cfg(target_family = "unix")` and on
supported platforms:

1. Replays `platform-checks` results via `platform_checks::emit_cfg()`
   and `emit_check_cfg()`.
2. cc-compiles `src/c/shim.c` into `libshim.a` with `-Dhas_symbol_X`
   for each detected intercept-family symbol.
3. Writes the symbol export list (`exports.map` on Linux,
   `exports.txt` on macOS) into `OUT_DIR`.
4. Emits cdylib link args:
   - Linux: `-Wl,--whole-archive`, `-Wl,--version-script=...`,
     `-Wl,-rpath,$ORIGIN`, `-fuse-ld=lld` (required; see Host
     requirements in the top-level `CLAUDE.md`).
   - macOS: `-Wl,-force_load,...`,
     `-Wl,-exported_symbols_list,...`, `-Wl,-rpath,@loader_path`.

`INTERCEPT_FAMILY` in `build.rs` lists the symbols `src/c/shim.c`
exports. Source of truth is `src/c/shim.c` itself.

Adding or removing an intercepted function is a three-place edit:

1. `src/c/shim.c` - the `EXPORT` wrapper itself (and the matching
   `extern` declaration of the `rust_<name>` implementation).
2. `intercept-preload/build.rs::INTERCEPT_FAMILY` - so the symbol
   passes the filter against `platform_checks::DETECTED_SYMBOLS`
   and is written into the version script / exports list.
3. `platform-checks/build.rs::SYMBOL_PROBES` - so the symbol is
   probed on the host at all. If it is not listed here it never
   enters `DETECTED_SYMBOLS`, and the `INTERCEPT_FAMILY` filter
   silently drops it, leaving the wrapper hidden by `local: *;`.

Miss any one of the three and the wrapper compiles but is invisible
to the dynamic linker, so libc handles the call instead of us.
Pairs that travel together (e.g. `popen`/`pclose`) each need their
own entry in all three places.

## Before modifying

- Changes here affect all intercepted builds on supported Unix platforms
- Test on multiple platforms if possible (CI covers Linux, macOS, Windows)
- The "doctor" logic that maintains interception across `exec` calls is subtle - read it fully before changing
- Reports go to a TCP collector - do not change the protocol without updating the collector in `bear`
