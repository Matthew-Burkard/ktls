//! Compiler errors and warnings beyond syntax and name resolution of types and imports.
//!
//! They are lint rules of the `compiler` category, so they share suppression, per-rule
//! configuration and quick fixes. Like the rest of the analysis they only report what is
//! certain: checks that need types run only for fully resolved modules, and every check
//! stays silent when part of the picture (smart casts, unknown lambda receivers, unresolved
//! supertypes) is missing.

mod declarations;
mod expressions;
mod flow;
mod jumps;
mod types;
mod usage;
mod when;

use super::{Cx, Sink};

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    declarations::check(cx, out);
    flow::check(cx, out);
    expressions::check(cx, out);
    when::check(cx, out);
    usage::check(cx, out);
    jumps::check(cx, out);
    types::check(cx, out);
}
