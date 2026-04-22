// Provider-agnostic cache + aggregation. Every file in this module is
// free of branches on which `Source` produced the data — they just
// consume canonical `ParsedSession`s from source::Source::parse_file.

mod agg;
mod build;
mod load;
mod schema;

pub use agg::{BLOCK_SECS, BreakdownKey, Bucket, BucketKey, BucketUsage, FilterOpts, aggregate};
pub use load::{LoadedCache, load};
