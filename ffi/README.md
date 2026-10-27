# bridge-solver C library

The bridge-solver engine as a C library, for programs in any language that
can call C. That's C and C++ directly; Python (`ctypes`), C#, Go, Java, Swift,
Julia and others go through their foreign-function interfaces. It provides:

- double-dummy tables;
- par;
- card-by-card play analysis;
- claim checking, meaning whether a claim holds however the unseen cards lie.

**Using Python?** `pip install bridge-solver` is easier than this library;
see [`python/README.md`](../python/README.md).

## What's in the download

Each release has `bridge-solver-c-<platform>.tar.gz` (`.zip` on Windows):

| File | |
|---|---|
| `include/bridge_solver.h` | the header: every function, documented |
| `lib/libbridgesolver.so`, `.dylib` | shared library (Linux, macOS) |
| `lib/libbridgesolver.a` | static library (Linux, macOS) |
| `lib/bridgesolver.dll` + `bridgesolver.dll.lib` | DLL and its import library (Windows) |
| `lib/bridgesolver.lib` | static library (Windows) |
| `examples/example.c` | a complete example, also the library's CI test |

## Building a program

**Linux**, shared:

```sh
cc myprog.c -Iinclude -Llib -lbridgesolver -Wl,-rpath,'$ORIGIN/lib'
```

**macOS**, shared. The library's install name is
`@rpath/libbridgesolver.dylib`, so give your program a run path:

```sh
cc myprog.c -Iinclude -Llib -lbridgesolver -Wl,-rpath,@executable_path/lib
```

**Static linking**, on either. Linux also needs the system libraries Rust
uses. These are what `rustc --print native-static-libs` reports for a glibc
target, and CI links the example this way:

```sh
cc myprog.c -Iinclude lib/libbridgesolver.a -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc  # Linux
cc myprog.c -Iinclude lib/libbridgesolver.a                                            # macOS
```

**Windows (MSVC)**: link `bridgesolver.dll.lib` and ship `bridgesolver.dll`
beside your program. To link the static `bridgesolver.lib` instead, add the
system libraries that `rustc --print native-static-libs` lists for the MSVC
target. Usually that's `ws2_32.lib userenv.lib ntdll.lib bcrypt.lib
advapi32.lib`, but CI does not test the static Windows build, so check
against your own toolchain.

## Using it

```c
#include "bridge_solver.h"

unsigned char tricks[20];
if (bs_dd_table_tricks("N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72",
                       tricks) == 0) {
    printf("North in spades: %d\n", tricks[0 * 5 + 3]);   /* rows N E S W, columns C D H S NT */
}

char *claim = bs_check_claim("{\"dealstr\": \"N:...\", \"trump\": \"S\", \"declarer\": \"N\","
                             " \"leader\": \"E\", \"plays\": [], \"total_tricks\": 10}");
/* {"verdict":"invalid","tricks_won":0,"tricks_left":13,"nodes":...} */
bs_free_string(claim);
```

Most functions take and return JSON. The requests and responses are the same
as the WebAssembly and Python packages', and the header documents each one.
The rules:

- **Strings** in and out are UTF-8.
- **Freeing.** Every returned `char *` is yours to free, with
  `bs_free_string()`. Never use `free()`.
- **Errors.** JSON-returning functions never return NULL. On failure they
  return `{"error": "..."}`.
- **Threads.** Every function is safe to call from several threads at once.
  The exception is a `BsAnalyzer`, which should be used by one thread at a
  time.

## Building it yourself

```sh
cargo build --release --manifest-path ffi/Cargo.toml   # into ffi/target/release
```

Inside the bridge-solver repository, build with
`./dev-build.sh --ci build --release --manifest-path ffi/Cargo.toml`. It keeps
`ffi/Cargo.lock` pinned to the published dependencies; see `CLAUDE.md`.

## Licence

MIT OR Apache-2.0. `THIRD-PARTY-NOTICES` lists the libraries compiled in.
