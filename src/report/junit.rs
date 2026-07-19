use super::{AggregateReport, ExitCode};

pub(super) fn render(report: &AggregateReport) -> String {
    let failures = report
        .flows
        .iter()
        .filter(|flow| flow.exit_code() == ExitCode::Automation)
        .count();
    let errors = report
        .flows
        .iter()
        .filter(|flow| !matches!(flow.exit_code(), ExitCode::Success | ExitCode::Automation))
        .count();
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites tests=\"{}\" failures=\"{failures}\" errors=\"{errors}\" time=\"{}.{:03}\">\n  <testsuite name=\"",
        report.flows.len(),
        report.duration_ms / 1000,
        report.duration_ms % 1000,
    );
    push_xml(&mut xml, &report.runner.name);
    xml.push_str(&format!(
        "\" tests=\"{}\" failures=\"{failures}\" errors=\"{errors}\" time=\"{}.{:03}\">\n",
        report.flows.len(),
        report.duration_ms / 1000,
        report.duration_ms % 1000,
    ));

    for flow in &report.flows {
        xml.push_str("    <testcase name=\"");
        push_xml(&mut xml, &flow.name);
        xml.push_str("\" classname=\"");
        push_xml(&mut xml, &flow.path);
        xml.push_str(&format!(
            "\" time=\"{}.{:03}\"",
            flow.duration_ms / 1000,
            flow.duration_ms % 1000,
        ));

        let exit_code = flow.exit_code();
        if exit_code == ExitCode::Success {
            xml.push_str(" />\n");
            continue;
        }

        let tag = if exit_code == ExitCode::Automation {
            "failure"
        } else {
            "error"
        };
        let controlling_failure = flow
            .failures
            .iter()
            .find(|failure| failure.exit_code() == exit_code);
        let kind = controlling_failure
            .map(|failure| failure.category.as_str())
            .unwrap_or(if exit_code == ExitCode::Interrupted {
                "interrupted"
            } else {
                "infrastructure"
            });
        let message = controlling_failure
            .map(|failure| failure.message.as_str())
            .unwrap_or(if exit_code == ExitCode::Interrupted {
                "flow interrupted"
            } else {
                "flow failed"
            });
        xml.push_str(">\n      <");
        xml.push_str(tag);
        xml.push_str(" type=\"");
        push_xml(&mut xml, kind);
        xml.push_str("\" message=\"");
        push_xml(&mut xml, message);
        xml.push_str("\">");
        if flow.failures.is_empty() {
            push_xml(&mut xml, message);
        } else {
            for (index, failure) in flow.failures.iter().enumerate() {
                if index != 0 {
                    xml.push('\n');
                }
                push_xml(&mut xml, failure.category.as_str());
                xml.push_str(": ");
                push_xml(&mut xml, failure.message.as_str());
            }
        }
        xml.push_str("</");
        xml.push_str(tag);
        xml.push_str(">\n    </testcase>\n");
    }
    xml.push_str("  </testsuite>\n</testsuites>\n");
    xml
}

fn push_xml(output: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&apos;"),
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
