# Vendored nx

Upstream: https://github.com/aarch64-switch-rs/nx (MIT, see LICENSE), commit
`5e0a67c630352c93414b421a77ed2de587f61393` (tag 0.6.0 + 3 commits). Only `src/`, `nx-derive/`, `Cargo.toml`,
`LICENSE`, `README.md` and `changelog.md` are copied.

Local changes (worth upstreaming):
1. `hbl::get_argv()`: rrt0 now stores the HBL `Argv` ABI entry instead of ignoring it (it was a `todo!`).
   nxlink passes the PC's address through argv, so stdout over nxlink needs this.
