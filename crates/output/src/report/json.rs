//! JSON reporter for the stable `metrics.v1.json` wire contract.
//!
//! The owned DTO lives in [`super::wire`] so report emission, comparison, and
//! baseline history all deserialize the same schema instead of maintaining
//! subtly different private views.

use crate::report::{Report, ReportError, Reporter};

/// Pretty-printed JSON reporter.
#[derive(Debug, Default, Clone, Copy)]
pub struct JsonReporter;

impl Reporter for JsonReporter {
    fn render(&self, report: &Report) -> Result<String, ReportError> {
        super::wire::render_pretty_json(report)
    }
}
