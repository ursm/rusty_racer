require "mkmf"
require "rb_sys/mkmf"

# The v8 crate downloads Deno's stock prebuilt rusty_v8 archive and links it
# statically — no build config needed here. As of rusty_v8 150.1.0 (PR #2008) the
# Linux archive is built shared-library-safe (v8_monolithic_for_shared_library),
# so it links into the extension cdylib without the R_X86_64_TPOFF32-under-shared
# error that used to force a from-source library-TLS build on linux.

create_rust_makefile("rusty_racer/rusty_racer")
