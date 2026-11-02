//! Data paths (section 4): the per-table reader feeding a writer through the byte budget, and
//! the pure encoders shared by the real writers and `testkit::RecordingTarget`. Owner:
//! copy-data.

pub mod cells;
pub mod mysql_encode;
pub mod pipeline;
pub mod values;
