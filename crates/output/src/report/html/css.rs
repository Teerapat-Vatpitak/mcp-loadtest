//! Inline CSS for the HTML reporter — extracted so the main `html.rs` stays
//! focused on the rendering pipeline. The styling is single-file by design:
//! the `report.html` output opens straight from `file://` with no CDN.

/// Inline stylesheet embedded in every rendered `report.html`. Stays as a
/// single `concat!` literal so the compiler can fold it into one static
/// string — there's no benefit to splitting per-rule.
pub(super) const CSS: &str = concat!(
    "body{font-family:-apple-system,BlinkMacSystemFont,\"Segoe UI\",Roboto,Arial,sans-serif;",
    "margin:0;padding:24px 32px;color:#1f2937;background:#f8fafc;line-height:1.45;}",
    ".wrap{max-width:1100px;margin:0 auto;}",
    "header{border-bottom:1px solid #e2e8f0;padding-bottom:16px;margin-bottom:24px;}",
    "h1{font-size:22px;margin:0 0 6px;color:#0f172a;}",
    "h2{font-size:16px;margin:32px 0 12px;color:#334155;border-bottom:1px solid #e2e8f0;padding-bottom:4px;}",
    ".meta{font-size:13px;color:#64748b;}",
    ".meta code{background:#f1f5f9;padding:1px 6px;border-radius:3px;font-size:12px;}",
    ".badge{display:inline-block;padding:3px 10px;border-radius:3px;font-weight:600;font-size:12px;text-transform:uppercase;}",
    ".badge.pass{background:#dcfce7;color:#166534;}.badge.fail{background:#fee2e2;color:#991b1b;}",
    "table{border-collapse:collapse;width:100%;font-size:13px;margin-top:8px;}",
    "th,td{text-align:left;padding:8px 12px;border-bottom:1px solid #e2e8f0;}",
    "th{background:#f1f5f9;color:#334155;font-weight:600;}",
    "td.num,th.num{text-align:right;font-family:ui-monospace,Menlo,Consolas,monospace;}",
    "tr.violation td{background:#fef2f2;color:#991b1b;}",
    ".chart{background:#fff;border:1px solid #e2e8f0;border-radius:4px;padding:12px;}",
    ".chart svg{display:block;width:100%;height:auto;}",
    "footer{margin-top:48px;padding-top:16px;border-top:1px solid #e2e8f0;color:#94a3b8;font-size:12px;}",
    ".mono{font-family:ui-monospace,Menlo,Consolas,monospace;}",
);
