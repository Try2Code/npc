// The only C++ in this project, and the only place Exiv2 is ever touched.
//
// THE INVARIANT: exactly one function here writes to disk -- write_sidecar --
// and it refuses any path not ending in ".xmp". Every other entry point opens
// its file read-only. Exiv2's writeMetadata() must appear nowhere else in this
// codebase; `grep -rn writeMetadata src/` is the audit.
#pragma once
#include "rust/cxx.h"

namespace npc {

// Shared structs are defined by the cxx-generated header. Forward declarations
// are enough for these declarations; shim.cc includes the generated header for
// the complete definitions.
struct FileMeta;
struct SidecarDoc;
struct PreviewBlob;

// Exiv2's XMP layer wraps the Adobe SDK and is not thread-safe. These bracket
// the process; the Rust side additionally serialises every call below behind a
// Mutex, so exclusion is structural rather than conventional.
void xmp_init();
void xmp_shutdown();

// --- read-only -----------------------------------------------------------
FileMeta read_meta(rust::Str path);
SidecarDoc read_sidecar(rust::Str xmp_path);
PreviewBlob extract_preview(rust::Str path, uint32_t min_dim);

// --- the only writer -----------------------------------------------------
// Asserts xmp_path ends in ".xmp" and throws otherwise. Preserves every
// property it did not put there: reads the existing packet, modifies only our
// keys in place, never constructs a fresh XmpData and never calls clear().
void write_sidecar(rust::Str xmp_path, const SidecarDoc &doc);

} // namespace npc
