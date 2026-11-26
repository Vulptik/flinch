# AMF ABI adapter

The engine owns capture, scheduling, buffers, timestamps, encoding settings and
output in Rust. `amf_bridge.cpp` only translates AMD's C++ interfaces to a small
C ABI. It avoids hand-written copies of AMD vtables in Rust. No private runtime
DLL is distributed and no FFmpeg code is used.

The unmodified public headers in `amf/` are AMD AMF SDK **1.5.0.0**, obtained from
the already installed MSYS2 `mingw-w64-x86_64-amf-headers` package.
Upstream: https://github.com/GPUOpen-LibrariesAndSDKs/AMF . Their MIT notices and
standards notices remain in each header. `amf/hashes.json` records SHA-256 hashes.
Ordinary builds never download or regenerate headers. The runtime is loaded
with `LOAD_LIBRARY_SEARCH_SYSTEM32`; its reported version must support this ABI.

Calls into the driver can block despite the requested timeout. The worker process
is supervised; no destructor in the supervisor waits for native driver teardown.
