//! `LoadNode`: render is `""`; all work happens at parse time in
//! `tags::parse` (delegating to the stock bridged `load` handler so the
//! library-discovery and subset-check messages stay stock).

use super::TagPayload;
use crate::render::context::Rctx;

/// `LoadNode.render`.
pub(crate) fn load(_rctx: &mut Rctx<'_>, _p: &TagPayload, _out: &mut String) {
}
