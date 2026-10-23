//! Compiler errors and warnings beyond syntax and name resolution of types and imports.
//!
//! They are lint rules of the `compiler` category, so they share suppression, per-rule
//! configuration and quick fixes. Like the rest of the analysis they only report what is
//! certain: checks that need types run only for fully resolved modules, and every check
//! stays silent when part of the picture (smart casts, unknown lambda receivers, unresolved
//! supertypes) is missing.

mod certain;
mod dataflow;
mod declarations;
mod expressions;
mod flow;
mod inheritance;
mod jumps;
mod markers;
mod signatures;
mod types;
mod typing;
mod usage;
mod when;

use super::{Cx, Sink};

pub(crate) use certain::certain_type;
pub(crate) use when::missing_branches;

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    declarations::check(cx, out);
    flow::check(cx, out);
    expressions::check(cx, out);
    when::check(cx, out);
    usage::check(cx, out);
    jumps::check(cx, out);
    types::check(cx, out);
    typing::check(cx, out);
    signatures::check(cx, out);
    inheritance::check(cx, out);
    markers::check(cx, out);
    dataflow::check(cx, out);
}
