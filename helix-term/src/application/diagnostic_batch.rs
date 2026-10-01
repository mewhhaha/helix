use std::collections::HashMap;

use helix_lsp::{jsonrpc, lsp, Call, LanguageServerId};

pub(super) enum PreparedMessage {
    Diagnostics(LanguageServerId, lsp::PublishDiagnosticsParams),
    Call(LanguageServerId, Call),
    Malformed(jsonrpc::Error),
}

/// Replace reports for the same server, URI and version within a ready batch.
/// Requests and other notifications are barriers; retained reports keep their
/// latest arrival position, including empty reports that clear diagnostics.
pub(super) fn coalesce(
    messages: impl IntoIterator<Item = (LanguageServerId, Call)>,
) -> Vec<PreparedMessage> {
    let mut output: Vec<Option<PreparedMessage>> = Vec::new();
    let mut reports = HashMap::new();
    for (server, call) in messages {
        let prepared = match call {
            Call::Notification(notification)
                if notification.method == "textDocument/publishDiagnostics" =>
            {
                match notification.params.parse::<lsp::PublishDiagnosticsParams>() {
                    Ok(params) => {
                        let key = (server, params.uri.clone(), params.version);
                        if let Some(previous) = reports.insert(key, output.len()) {
                            output[previous] = None;
                        }
                        PreparedMessage::Diagnostics(server, params)
                    }
                    Err(error) => {
                        reports.clear();
                        PreparedMessage::Malformed(error)
                    }
                }
            }
            call => {
                reports.clear();
                PreparedMessage::Call(server, call)
            }
        };
        output.push(Some(prepared));
    }
    output.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn report(uri: &str, version: i32, message: Option<&str>) -> Call {
        let diagnostics = message.map(|message| vec![json!({"range": {"start":{"line":0,"character":0}, "end":{"line":0,"character":1}}, "message":message})]).unwrap_or_default();
        serde_json::from_value(json!({"method":"textDocument/publishDiagnostics", "params":{"uri":uri,"version":version,"diagnostics":diagnostics}})).unwrap()
    }

    #[test]
    fn preserves_latest_position_versions_uris_and_empty_reports() {
        let server = LanguageServerId::default();
        let prepared = coalesce([
            (server, report("file:///a", 1, Some("old"))),
            (server, report("file:///b", 1, Some("other"))),
            (server, report("file:///a", 2, Some("newer version"))),
            (server, report("file:///a", 1, None)),
        ]);
        let reports: Vec<_> = prepared
            .into_iter()
            .map(|report| match report {
                PreparedMessage::Diagnostics(_, report) => report,
                _ => panic!(),
            })
            .collect();
        assert_eq!(reports.len(), 3);
        assert_eq!(reports[0].uri.as_str(), "file:///b");
        assert_eq!(reports[1].version, Some(2));
        assert!(reports[2].diagnostics.is_empty());
    }

    #[test]
    fn requests_and_malformed_notifications_are_barriers() {
        let server = LanguageServerId::default();
        for barrier in [
            serde_json::from_value(json!({"id":1,"method":"workspace/applyEdit","params":{}}))
                .unwrap(),
            serde_json::from_value(json!({"method":"textDocument/publishDiagnostics","params":{}}))
                .unwrap(),
        ] {
            assert_eq!(
                coalesce([
                    (server, report("file:///a", 1, Some("before"))),
                    (server, barrier),
                    (server, report("file:///a", 1, Some("after")))
                ])
                .len(),
                3
            );
        }
    }
}
