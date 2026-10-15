//! The test root: every module, so `zig build test` covers all of them.
//!
//! The readers and the decoders keep their tests beside the code they check,
//! because an off-by-one in a bit-packed group is a wrong value rather than a
//! failure, and a wrong value in a comparison tool is the worst kind of bug
//! there is.
//!
//! This used to say the engine's own tests are in `csvdiff.zig`. There are none:
//! that file has no `test` block, and it is not imported here, so anything added
//! to it would not run either. What covers the engine is `test.sh` and the parity
//! workflows, which check this port's answers against the other three rather than
//! its parts against themselves. Worth knowing before trusting `zig build test`
//! to have exercised a change to the join.
//!
//! `pqdiff.zig` is the exception, for its value rules only: what a Parquet cell
//! normalises to and when it counts as absent, which is small enough to state
//! beside the code and was wrong without any end-to-end input to show it.

test {
    _ = @import("scan.zig");
    _ = @import("field.zig");
    _ = @import("slab.zig");
    _ = @import("text.zig");
    _ = @import("thrift.zig");
    _ = @import("codec.zig");
    _ = @import("encoding.zig");
    _ = @import("pqread.zig");
    _ = @import("pqdiff.zig");
}
