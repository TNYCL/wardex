//! rmcp 3.5 supports MCP 2.0 but does not yet serialize `capabilities.events`.
//! Amend discovery responses at the wire boundary only; all request parsing,
//! authentication, metadata checks and tool handling remain in rmcp.

use std::io;

use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

const MAX_FRAME: usize = 10 * 1024 * 1024;

fn advertise(value: &mut Value) {
    let Some(result) = value.get_mut("result") else {
        return;
    };
    let discovery = result.get("supportedVersions").is_some_and(Value::is_array);
    let legacy = result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .is_some_and(|v| v >= "2026-07-28")
        && result.get("serverInfo").is_some_and(Value::is_object);
    if (discovery || legacy)
        && let Some(capabilities) = result
            .get_mut("capabilities")
            .and_then(Value::as_object_mut)
    {
        capabilities.insert("events".into(), json!({}));
    }
}

pub(crate) async fn http_capabilities(req: Request, next: Next) -> Response {
    let discovery = req
        .headers()
        .get("mcp-method")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|method| matches!(method, "server/discover" | "initialize"));
    let response = next.run(req).await;
    if !discovery {
        return response;
    }
    // Only JSON responses can contain discovery. Errors and other content pass through.
    if !response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"application/json"))
    {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_FRAME).await else {
        return (StatusCode::BAD_GATEWAY, "MCP response exceeds frame limit").into_response();
    };
    let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    advertise(&mut value);
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(value.to_string()))
}

pub(crate) async fn rewrite_stdio(
    input: impl AsyncRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
) -> io::Result<()> {
    let mut input = BufReader::new(input);
    let mut frame = Vec::new();
    loop {
        let available = input.fill_buf().await?;
        if available.is_empty() {
            if !frame.is_empty() {
                return Err(io::Error::other("incomplete MCP response"));
            }
            return output.flush().await;
        }
        let n = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |p| p + 1);
        if frame.len() + n > MAX_FRAME {
            return Err(io::Error::other("MCP response exceeds frame limit"));
        }
        let complete = available[n - 1] == b'\n';
        frame.extend_from_slice(&available[..n]);
        input.consume(n);
        if complete {
            let mut value: Value = serde_json::from_slice(&frame)
                .map_err(|_| io::Error::other("invalid MCP response"))?;
            advertise(&mut value);
            output.write_all(value.to_string().as_bytes()).await?;
            output.write_all(b"\n").await?;
            output.flush().await?;
            frame.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{ServerCapabilities, ServerConfig};
    use rmcp::{ServerHandler, ServiceExt};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    #[test]
    fn only_discovery_is_amended() {
        let mut discover =
            json!({"result":{"supportedVersions":["2026-07-28"],"capabilities":{"tools":{}}}});
        advertise(&mut discover);
        assert_eq!(discover["result"]["capabilities"]["events"], json!({}));
        assert_eq!(discover["result"]["capabilities"]["tools"], json!({}));
        let mut tool = json!({"result":{"content":[],"capabilities":{"tools":{}}}});
        let original = tool.clone();
        advertise(&mut tool);
        assert_eq!(tool, original);
    }

    #[tokio::test]
    async fn real_rmcp_discovery_survives_stdio_adapter() {
        struct Server;
        impl ServerHandler for Server {
            fn get_info(&self) -> ServerConfig {
                ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            }
        }
        let (mut client_input, server_input) = tokio::io::duplex(8192);
        let (server_output, adapter_input) = tokio::io::duplex(8192);
        let (adapter_output, client_output) = tokio::io::duplex(8192);
        let server = tokio::spawn(async move {
            let service = Server.serve((server_input, server_output)).await.unwrap();
            let _ = service.waiting().await;
        });
        let adapter = tokio::spawn(rewrite_stdio(adapter_input, adapter_output));
        let request = json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{
            "io.modelcontextprotocol/protocolVersion":"2026-07-28",
            "io.modelcontextprotocol/clientInfo":{"name":"contract-test","version":"1"},
            "io.modelcontextprotocol/clientCapabilities":{}
        }}});
        client_input
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            BufReader::new(client_output).read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], 1);
        assert!(
            response["result"]["supportedVersions"]
                .as_array()
                .unwrap()
                .contains(&json!("2026-07-28"))
        );
        assert_eq!(response["result"]["capabilities"]["events"], json!({}));
        server.abort();
        adapter.abort();
    }
}
