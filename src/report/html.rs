use super::{AggregateReport, AggregateStatus, FlowStatus};

pub(super) fn render(report: &AggregateReport) -> String {
    let (status, status_class) = match report.status {
        AggregateStatus::Passed => ("Passed", "passed"),
        AggregateStatus::Failed => ("Failed", "failed"),
        AggregateStatus::Interrupted => ("Interrupted", "interrupted"),
    };
    let passed = report
        .flows
        .iter()
        .filter(|flow| flow.status == FlowStatus::Passed)
        .count();
    let failed = report
        .flows
        .iter()
        .filter(|flow| flow.status == FlowStatus::Failed)
        .count();
    let interrupted = report.flows.len() - passed - failed;
    let mut html = String::from(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Playrust report</title><style>\n:root{color-scheme:light dark;font-family:ui-sans-serif,system-ui,sans-serif;line-height:1.5}body{max-width:70rem;margin:0 auto;padding:2rem;background:#f5f5f4;color:#1c1917}header,.flow{background:#fff;border:1px solid #d6d3d1;border-radius:.6rem;padding:1.25rem;margin-bottom:1rem}.summary{display:flex;gap:1rem;flex-wrap:wrap}.badge{display:inline-block;border-radius:999px;padding:.2rem .65rem;font-weight:700}.passed{background:#dcfce7;color:#166534}.failed{background:#fee2e2;color:#991b1b}.interrupted{background:#fef3c7;color:#92400e}h1,h2,h3{line-height:1.2}h2{overflow-wrap:anywhere}.meta{color:#57534e}.failure{border-left:.3rem solid #dc2626;padding-left:1rem;margin:1rem 0}dl{display:grid;grid-template-columns:max-content 1fr;gap:.25rem 1rem}dt{font-weight:700}dd{margin:0;overflow-wrap:anywhere}code{font-family:ui-monospace,monospace;overflow-wrap:anywhere}@media(prefers-color-scheme:dark){body{background:#0c0a09;color:#fafaf9}header,.flow{background:#1c1917;border-color:#44403c}.meta{color:#d6d3d1}}\n</style></head><body><header><h1>Playrust report</h1><p><span class=\"badge ",
    );
    html.push_str(status_class);
    html.push_str("\">");
    html.push_str(status);
    html.push_str("</span></p><div class=\"summary\"><strong>");
    html.push_str(&format!("{} flow(s)</strong><span>{passed} passed</span><span>{failed} failed</span><span>{interrupted} interrupted</span><span>{} ms</span><span>Exit code {}</span></div><p class=\"meta\">Runner: ", report.flows.len(), report.duration_ms, report.exit_code));
    push_html(&mut html, &report.runner.name);
    html.push(' ');
    push_html(&mut html, &report.runner.version);
    html.push_str(" | Schema version ");
    html.push_str(&report.schema_version.to_string());
    if let Some(chromium) = &report.chromium {
        html.push_str(" | Chromium ");
        push_html(&mut html, &chromium.version);
        html.push_str(" (");
        push_html(&mut html, &chromium.executable);
        html.push(')');
    }
    html.push_str("</p></header><main>");

    for flow in &report.flows {
        let (flow_status, flow_class) = match flow.status {
            FlowStatus::Passed => ("Passed", "passed"),
            FlowStatus::Failed => ("Failed", "failed"),
            FlowStatus::Interrupted => ("Interrupted", "interrupted"),
        };
        html.push_str("<section class=\"flow\"><p><span class=\"badge ");
        html.push_str(flow_class);
        html.push_str("\">");
        html.push_str(flow_status);
        html.push_str("</span></p><h2>");
        push_html(&mut html, &flow.name);
        html.push_str("</h2><dl><dt>Flow path</dt><dd><code>");
        push_html(&mut html, &flow.path);
        html.push_str("</code></dd><dt>Duration</dt><dd>");
        html.push_str(&flow.duration_ms.to_string());
        html.push_str(" ms</dd></dl>");

        for failure in &flow.failures {
            html.push_str("<div class=\"failure\"><h3>");
            push_html(&mut html, failure.category.as_str());
            html.push_str("</h3><p>");
            push_html(&mut html, failure.message.as_str());
            html.push_str("</p><dl>");
            if let Some(step) = &failure.step {
                html.push_str("<dt>Step</dt><dd>");
                html.push_str(&step.number.to_string());
                html.push_str(": ");
                push_html(&mut html, &step.operation);
                if let Some(id) = &step.id {
                    html.push_str(" (id: ");
                    push_html(&mut html, id);
                    html.push(')');
                }
                html.push_str("</dd>");
                if let Some(locator) = &step.locator {
                    html.push_str("<dt>Locator</dt><dd><code>");
                    push_html(&mut html, locator.as_str());
                    html.push_str("</code></dd>");
                }
            }
            if let Some(url) = &failure.current_url {
                html.push_str("<dt>Current URL</dt><dd><code>");
                push_html(&mut html, url.as_str());
                html.push_str("</code></dd>");
            }
            if let Some(timeout) = failure.timeout_ms {
                html.push_str("<dt>Timeout</dt><dd>");
                html.push_str(&timeout.to_string());
                html.push_str(" ms</dd>");
            }
            if let Some(observed) = &failure.last_observed {
                html.push_str("<dt>Last observed</dt><dd>");
                push_html(&mut html, observed.as_str());
                html.push_str("</dd>");
            }
            html.push_str("</dl></div>");
        }

        html.push_str("<h3>Artifacts</h3><dl><dt>Directory</dt><dd><code>");
        push_html(&mut html, &flow.artifacts.directory);
        html.push_str("</code></dd>");
        for path in &flow.artifacts.screenshots {
            push_html_path(&mut html, "Screenshot", path);
        }
        if let Some(path) = &flow.artifacts.failure_screenshot {
            push_html_path(&mut html, "Failure screenshot", path);
        }
        if let Some(path) = &flow.artifacts.visual_actual {
            push_html_path(&mut html, "Visual actual", path);
        }
        if let Some(path) = &flow.artifacts.visual_diff {
            push_html_path(&mut html, "Visual diff", path);
        }
        if let Some(path) = &flow.artifacts.recording {
            push_html_path(&mut html, "Recording", path);
        }
        if let Some(path) = &flow.artifacts.partial_recording {
            push_html_path(&mut html, "Partial recording", path);
        }
        html.push_str("</dl></section>");
    }
    html.push_str("</main></body></html>\n");
    html
}

fn push_html_path(output: &mut String, label: &str, path: &str) {
    output.push_str("<dt>");
    output.push_str(label);
    output.push_str("</dt><dd><code>");
    push_html(output, path);
    output.push_str("</code></dd>");
}

fn push_html(output: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            '\u{9}'
            | '\u{A}'
            | '\u{D}'
            | '\u{20}'..='\u{D7FF}'
            | '\u{E000}'..='\u{FFFD}'
            | '\u{10000}'..='\u{10FFFF}' => output.push(character),
            _ => output.push('\u{FFFD}'),
        }
    }
}
