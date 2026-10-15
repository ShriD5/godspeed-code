//! Server side: dispatch loop over string frames + the WebSocket acceptor.

use std::collections::HashMap;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as HandshakeRequest, Response as HandshakeResponse,
};
use tokio_tungstenite::tungstenite::http::StatusCode;

use crate::{ClientFrame, RpcError, RpcReply, RpcService, ServerFrame};

/// Serve one connection: read client frames from `inbound`, write server frames to `out`.
/// Returns when `inbound` closes; all in-flight request tasks are aborted on exit.
pub async fn serve_connection(
    service: Arc<dyn RpcService>,
    out: mpsc::Sender<String>,
    mut inbound: mpsc::Receiver<String>,
) {
    let mut running: HashMap<u64, tokio::task::AbortHandle> = HashMap::new();
    while let Some(payload) = inbound.recv().await {
        // ndjson: a transport may batch several frames per message.
        for line in payload.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let frame: ClientFrame = match serde_json::from_str(line) {
                Ok(frame) => frame,
                Err(err) => {
                    tracing::warn!(error = %err, "rpc: dropping malformed client frame");
                    continue;
                }
            };
            running.retain(|_, task| !task.is_finished());
            if frame.cancel {
                if let Some(task) = running.remove(&frame.id) {
                    task.abort();
                }
                continue;
            }
            let Some(method) = frame.method else {
                tracing::warn!(id = frame.id, "rpc: frame has neither method nor cancel");
                continue;
            };
            let task = tokio::spawn(handle_request(
                service.clone(),
                out.clone(),
                frame.id,
                method,
                frame.params,
            ));
            running.insert(frame.id, task.abort_handle());
        }
    }
    for (_, task) in running {
        task.abort();
    }
}

async fn handle_request(
    service: Arc<dyn RpcService>,
    out: mpsc::Sender<String>,
    id: u64,
    method: String,
    params: serde_json::Value,
) {
    let send = |frame: ServerFrame| {
        let out = out.clone();
        async move {
            match serde_json::to_string(&frame) {
                Ok(json) => out.send(json).await.map_err(|_| RpcError::Closed),
                Err(err) => {
                    tracing::error!(error = %err, "rpc: failed to serialize server frame");
                    Err(RpcError::Closed)
                }
            }
        }
    };
    match service.handle(&method, params).await {
        Ok(RpcReply::Value(value)) => {
            let _ = send(ServerFrame {
                id,
                ok: Some(value),
                ..Default::default()
            })
            .await;
        }
        Ok(RpcReply::Stream(mut stream)) => {
            // Only the versioned checkout-PR stream uses an explicit readiness
            // frame. Sending it for legacy streams would make older clients remove
            // their pending stream as if it were a unary response.
            if method == crate::methods::WATCH_CHECKOUT_CHANGE_REQUEST
                && send(ServerFrame {
                    id,
                    ok: Some(serde_json::json!({ "stream": true })),
                    ..Default::default()
                })
                .await
                .is_err()
            {
                return;
            }
            while let Some(item) = stream.next().await {
                if send(ServerFrame {
                    id,
                    item: Some(item),
                    ..Default::default()
                })
                .await
                .is_err()
                {
                    return; // connection gone
                }
            }
            let _ = send(ServerFrame {
                id,
                done: true,
                ..Default::default()
            })
            .await;
        }
        Err(err) => {
            let _ = send(ServerFrame {
                id,
                err: Some(err.to_string()),
                ..Default::default()
            })
            .await;
        }
    }
}

/// Accept WebSocket connections forever, serving each with `service`.
pub async fn serve_ws_listener(listener: TcpListener, service: Arc<dyn RpcService>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tracing::debug!(%peer, "rpc: connection accepted");
                tokio::spawn(serve_ws_socket(stream, service.clone()));
            }
            Err(err) => {
                tracing::warn!(error = %err, "rpc: accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// Largest HTTP body the listener accepts (webhook payloads).
pub const MAX_HTTP_BODY: usize = 1024 * 1024;
/// Largest request head we wait for before deciding WebSocket vs plain HTTP.
const MAX_HTTP_HEAD: usize = 16 * 1024;
const HTTP_HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Peek (without consuming) until the request head is complete, so a
/// WebSocket handshake still reaches tungstenite byte-for-byte. `None` when
/// the peer sends no complete head in time.
async fn peek_head(stream: &TcpStream) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; MAX_HTTP_HEAD];
    let deadline = tokio::time::Instant::now() + HTTP_HEAD_TIMEOUT;
    loop {
        let n = tokio::time::timeout_at(deadline, stream.peek(&mut buf))
            .await
            .ok()?
            .ok()?;
        if n == 0 {
            return None;
        }
        if let Some(end) = buf[..n].windows(4).position(|w| w == b"\r\n\r\n") {
            buf.truncate(end + 4);
            return Some(buf);
        }
        if n == buf.len() {
            return None;
        }
        // `peek` returns the same bytes until more arrive; a handshake split
        // across segments is rare enough that a short nap beats a reader shim.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
    }
}

fn parse_head(head: &[u8]) -> Option<crate::HttpRequest> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.to_string();
    let headers = lines
        .filter(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    Some(crate::HttpRequest {
        method,
        path,
        headers,
        body: Vec::new(),
    })
}

fn is_websocket_upgrade(request: &crate::HttpRequest) -> bool {
    request
        .header("upgrade")
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

async fn serve_http(
    mut stream: TcpStream,
    head_len: usize,
    mut request: crate::HttpRequest,
    service: Arc<dyn RpcService>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let response = 'response: {
        let mut head = vec![0u8; head_len];
        if stream.read_exact(&mut head).await.is_err() {
            return;
        }
        let length = request
            .header("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        if length > MAX_HTTP_BODY {
            break 'response crate::HttpResponse::json(
                413,
                &serde_json::json!({ "error": "payload too large" }),
            );
        }
        // Read the body even for a request we refuse: closing with unread
        // bytes queued makes the kernel reset the connection, and the caller
        // would never see the status.
        let mut body = vec![0u8; length];
        match tokio::time::timeout(HTTP_HEAD_TIMEOUT, stream.read_exact(&mut body)).await {
            Ok(Ok(_)) => {}
            _ => return,
        }
        // Same stance as the WebSocket path: no browser page gets in.
        if request.header("origin").is_some() {
            break 'response crate::HttpResponse::json(
                403,
                &serde_json::json!({ "error": "origin not allowed on local IPC" }),
            );
        }
        request.body = body;
        service.handle_http(request).await
    };
    let reason = match response.status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let bytes = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.status,
        response.body.len(),
        response.body
    );
    let _ = stream.write_all(bytes.as_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn serve_ws_socket(stream: TcpStream, service: Arc<dyn RpcService>) {
    // Plain HTTP (agent webhooks) shares the port: anything that doesn't ask
    // for a WebSocket upgrade goes to `RpcService::handle_http`.
    if let Some(head) = peek_head(&stream).await
        && let Some(request) = parse_head(&head)
        && !is_websocket_upgrade(&request)
    {
        serve_http(stream, head.len(), request, service).await;
        return;
    }
    // Native viewports dial this socket with a bare `connect_async` and send
    // no `Origin` header. A browser always attaches `Origin` to a WebSocket
    // handshake and cannot forge or suppress it from script, and WebSockets
    // are exempt from the Same-Origin Policy — so only rejecting any handshake
    // that carries `Origin` keeps a page the user happens to visit from
    // reaching this local socket. Keep this check.
    //
    // The large `Err` (ErrorResponse) is the shape tungstenite's Callback
    // trait requires; it can't be boxed away here.
    #[allow(clippy::result_large_err)]
    let reject_cross_origin = |req: &HandshakeRequest, resp: HandshakeResponse| {
        if let Some(origin) = req.headers().get("origin") {
            tracing::warn!(
                origin = %String::from_utf8_lossy(origin.as_bytes()),
                "rpc: rejecting handshake carrying an Origin header (cross-origin browser dial)"
            );
            let mut err = ErrorResponse::new(Some("origin not allowed on local IPC".to_string()));
            *err.status_mut() = StatusCode::FORBIDDEN;
            return Err(err);
        }
        Ok(resp)
    };
    let ws = match tokio_tungstenite::accept_hdr_async(stream, reject_cross_origin).await {
        Ok(ws) => ws,
        Err(err) => {
            tracing::warn!(error = %err, "rpc: websocket handshake failed");
            return;
        }
    };
    let (mut sink, mut ws_stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
    let (in_tx, in_rx) = mpsc::channel::<String>(256);

    // Pump: socket <-> string channels. Ends when either side closes.
    let pump = tokio::spawn(async move {
        loop {
            tokio::select! {
                frame = out_rx.recv() => match frame {
                    Some(text) => {
                        if sink.send(WsMessage::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        let _ = sink.send(WsMessage::Close(None)).await;
                        break;
                    }
                },
                message = ws_stream.next() => match message {
                    Some(Ok(WsMessage::Text(text))) => {
                        if in_tx.send(text).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {} // ping/pong/binary — ignored
                },
            }
        }
    });

    serve_connection(service, out_tx, in_rx).await;
    pump.abort();
}
