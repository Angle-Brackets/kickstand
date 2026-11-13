# libkickstand

Link it into your Switch homebrew so Kickstand's `reload_nro` can swap in a new build in the same hbloader
process quickly and safely:

- **Quit on request:** the app quits when Kickstand asks, with no button press.
- **The next-load hook:** hbloader loads the new build directly instead of hbmenu.
- **Cleanup:** threads still running are paused for good, and the thread stacks and JIT memory a build leaves
  behind are released. Otherwise they can crash the next build.

| Your homebrew | Use | How |
| - | - | - |
| C/C++ on libnx | `libkickstand.a` (this folder; `make` with devkitA64) | Linker flags only, nothing to call (`kickstand.mk`) |
| Rust on nx | the `kickstand-nx` crate (`rust/`) | `kickstand::quit_requested()`, `kickstand::spawn()`, `kickstand::finish()` |

The details, with Makefile and CMake examples, are in [TOOLS.md](../TOOLS.md#libkickstand). Both are
GPL-3.0-only, like the rest of Kickstand.
