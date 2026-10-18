use std::collections::VecDeque;

use input_event::scancode::Linux;

use super::*;

/// answers agent requests from a script and records them
#[derive(Default)]
struct Fake {
    requests: Vec<(String, Request)>,
    responses: VecDeque<Result<Response, String>>,
    targets: Vec<AgentTarget>,
}

impl Service for Fake {
    async fn agent(&mut self, target: &str, request: Request) -> Result<Response, String> {
        self.requests.push((target.to_owned(), request));
        self.responses
            .pop_front()
            .unwrap_or(Ok(Response::Ok { frame: None }))
    }

    async fn targets(&mut self) -> Result<Vec<AgentTarget>, String> {
        Ok(self.targets.clone())
    }
}

fn server(responses: impl IntoIterator<Item = Result<Response, String>>) -> Server<Fake> {
    let fake = Fake {
        responses: responses.into_iter().collect(),
        ..Fake::default()
    };
    Server::new(fake, DEFAULT_MAX_EDGE)
}

async fn rpc(server: &mut Server<Fake>, method: &str, params: Value) -> Value {
    let line = json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params});
    let answer = server
        .handle(line.to_string().as_bytes())
        .await
        .expect("a request is answered");
    assert_eq!(answer["id"], 7);
    assert_eq!(answer["jsonrpc"], "2.0");
    answer
}

async fn call(server: &mut Server<Fake>, tool: &str, arguments: Value) -> Value {
    rpc(
        server,
        "tools/call",
        json!({"name": tool, "arguments": arguments}),
    )
    .await
    .get("result")
    .cloned()
    .expect("tool calls have a result")
}

fn frame(scale: f64, x: i32, y: i32) -> Frame {
    Frame {
        format: Format::Png,
        width: 1568,
        height: 822,
        scale,
        x,
        y,
        data: Blob(b"\x89PNG pixels".to_vec()),
    }
}

fn texts(result: &Value) -> String {
    result["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["type"] == "text")
        .map(|c| c["text"].as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn initialize_answers_the_clients_version_when_supported() {
    let mut s = server([]);
    for version in ["2025-06-18", "2025-03-26", "2024-11-05"] {
        let answer = rpc(
            &mut s,
            "initialize",
            json!({"protocolVersion": version, "capabilities": {}}),
        )
        .await;
        assert_eq!(answer["result"]["protocolVersion"], version);
        assert!(answer["result"]["capabilities"]["tools"].is_object());
        assert_eq!(answer["result"]["serverInfo"]["name"], "mimouse");
    }
    let answer = rpc(
        &mut s,
        "initialize",
        json!({"protocolVersion": "2099-01-01"}),
    )
    .await;
    assert_eq!(answer["result"]["protocolVersion"], PROTOCOL_VERSION);
    let answer = rpc(&mut s, "initialize", json!({})).await;
    assert_eq!(answer["error"]["code"], INVALID_PARAMS);
}

#[tokio::test]
async fn tools_list_describes_every_tool_with_an_object_schema() {
    let mut s = server([]);
    let answer = rpc(&mut s, "tools/list", json!({})).await;
    let tools = answer["result"]["tools"].as_array().unwrap();
    let names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "list_targets",
            "session_open",
            "session_close",
            "screenshot",
            "pointer_to",
            "click",
            "key",
            "type",
            "scroll",
            "clip_get",
            "clip_set"
        ]
    );
    for tool in tools {
        let name = &tool["name"];
        assert_eq!(tool["inputSchema"]["type"], "object", "{name}");
        assert!(tool["inputSchema"]["properties"].is_object(), "{name}");
        assert!(!tool["description"].as_str().unwrap().is_empty(), "{name}");
        assert_eq!(tool["annotations"]["openWorldHint"], false, "{name}");
        let read_only =
            ["list_targets", "screenshot", "clip_get"].contains(&name.as_str().unwrap());
        assert_eq!(tool["annotations"]["readOnlyHint"], read_only, "{name}");
        // every listed tool is callable (unknown tools are protocol errors)
        let answer = rpc(&mut s, "tools/call", json!({"name": name, "arguments": {}})).await;
        assert!(answer["result"].is_object(), "{name}: {answer}");
    }
}

#[tokio::test]
async fn screenshot_returns_an_image_and_becomes_the_coordinate_reference() {
    let mut s = server([Ok(Response::Frame(frame(2.0, 10, 20)))]);
    let result = call(
        &mut s,
        "screenshot",
        json!({"target": "megapc", "format": "png"}),
    )
    .await;
    assert_eq!(result["isError"], false);
    let image = result["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "image")
        .expect("image content");
    assert_eq!(image["mimeType"], "image/png");
    assert_eq!(
        STANDARD.decode(image["data"].as_str().unwrap()).unwrap(),
        b"\x89PNG pixels"
    );
    assert!(texts(&result).contains("1568x822"));
    let (target, request) = &s.service.requests[0];
    assert_eq!(target, "megapc");
    assert_eq!(
        *request,
        Request::Screenshot(Shot {
            region: None,
            max_edge: Some(DEFAULT_MAX_EDGE),
            format: Format::Png,
        })
    );
    let stored = &s.frames["megapc"];
    assert_eq!((stored.scale, stored.x, stored.y), (2.0, 10, 20));
    assert!(stored.data.0.is_empty(), "pixels are not kept");
}

#[tokio::test]
async fn pointer_to_maps_image_pixels_through_the_last_frame() {
    let mut s = server([Ok(Response::Frame(frame(3.11, 100, 0)))]);
    call(&mut s, "screenshot", json!({})).await;
    let result = call(&mut s, "pointer_to", json!({"x": 784, "y": 411})).await;
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(
        s.service.requests[1].1,
        Request::PointerTo {
            x: 100 + 2438,
            y: 1278,
            observe: None
        }
    );
    // desktop coordinates pass through unchanged
    call(
        &mut s,
        "pointer_to",
        json!({"x": 5, "y": 6, "coords": "desktop"}),
    )
    .await;
    assert_eq!(
        s.service.requests[2].1,
        Request::PointerTo {
            x: 5,
            y: 6,
            observe: None
        }
    );
    // frames are per target
    let result = call(
        &mut s,
        "pointer_to",
        json!({"target": "megapc", "x": 1, "y": 1}),
    )
    .await;
    assert_eq!(result["isError"], true);
}

#[tokio::test]
async fn pointer_to_without_a_frame_asks_for_a_screenshot() {
    let mut s = server([]);
    let result = call(&mut s, "pointer_to", json!({"x": 10, "y": 10})).await;
    assert_eq!(result["isError"], true);
    assert!(
        texts(&result).contains("Take a screenshot first"),
        "{result}"
    );
    assert!(s.service.requests.is_empty());
}

#[tokio::test]
async fn image_coordinates_outside_the_frame_are_refused() {
    let mut s = server([Ok(Response::Frame(frame(1.0, 0, 0)))]);
    call(&mut s, "screenshot", json!({})).await;
    let result = call(&mut s, "click", json!({"x": 1568, "y": 10})).await;
    assert_eq!(result["isError"], true);
    assert!(texts(&result).starts_with("bad_request"), "{result}");
    assert_eq!(s.service.requests.len(), 1);
}

#[tokio::test]
async fn preempted_is_a_tool_error_telling_the_model_to_stop() {
    let mut s = server([Ok(Response::error(
        ErrorCode::Preempted,
        "a person used the keyboard",
    ))]);
    let result = call(&mut s, "key", json!({"keys": "ctrl+l"})).await;
    assert_eq!(result["isError"], true);
    let text = texts(&result);
    assert!(
        text.starts_with("preempted: a person used the keyboard"),
        "{text}"
    );
    assert!(text.contains("ask the user"), "{text}");
    assert_eq!(
        s.service.requests[0].1,
        Request::Key {
            keys: vec![Linux::KeyLeftCtrl, Linux::KeyL],
            observe: None
        }
    );
}

#[tokio::test]
async fn service_failures_are_tool_errors() {
    let mut s = server([Err("cannot reach the MiMouse service".into())]);
    let result = call(&mut s, "scroll", json!({"dy": 3})).await;
    assert_eq!(result["isError"], true);
    assert!(texts(&result).contains("cannot reach the MiMouse service"));
}

#[tokio::test]
async fn click_at_a_point_moves_then_clicks_and_returns_the_observed_frame() {
    let mut s = server([
        Ok(Response::Frame(frame(2.0, 0, 0))),
        Ok(Response::Ok { frame: None }),
        Ok(Response::Ok {
            frame: Some(frame(1.5, 40, 0)),
        }),
    ]);
    call(&mut s, "screenshot", json!({})).await;
    let result = call(
        &mut s,
        "click",
        json!({"x": 10, "y": 20, "count": 2, "observe": {"settle_ms": 500}}),
    )
    .await;
    assert_eq!(result["isError"], false, "{result}");
    let content = result["content"].as_array().unwrap();
    assert_eq!(content[0], json!({"type": "text", "text": "ok"}));
    assert_eq!(content.last().unwrap()["type"], "image");
    assert_eq!(
        s.service.requests[1].1,
        Request::PointerTo {
            x: 20,
            y: 40,
            observe: None
        }
    );
    let Request::Click {
        button,
        count,
        observe: Some(observe),
    } = &s.service.requests[2].1
    else {
        panic!("expected an observed click: {:?}", s.service.requests[2]);
    };
    assert_eq!((*button, *count), (Button::Left, 2));
    assert_eq!(observe.settle_ms, Some(500));
    assert_eq!(observe.shot.max_edge, Some(DEFAULT_MAX_EDGE));
    // the observed frame is the new reference
    let stored = &s.frames["self"];
    assert_eq!((stored.scale, stored.x), (1.5, 40));
}

#[tokio::test]
async fn click_rejects_counts_outside_one_to_three() {
    let mut s = server([]);
    for count in [0, 4] {
        let result = call(&mut s, "click", json!({"count": count})).await;
        assert_eq!(result["isError"], true);
    }
    assert!(s.service.requests.is_empty());
}

#[tokio::test]
async fn type_reports_unmapped_characters() {
    let mut s = server([Ok(Response::Typed {
        unmapped: "€".into(),
        frame: None,
    })]);
    let result = call(&mut s, "type", json!({"text": "5 €", "observe": false})).await;
    assert_eq!(result["isError"], false);
    assert!(texts(&result).contains("unmapped"), "{result}");
    assert!(texts(&result).contains('€'));
}

#[tokio::test]
async fn clipboard_text_round_trips_as_text() {
    let mut s = server([
        Ok(Response::Ok { frame: None }),
        Ok(Response::Clip {
            mime: "text/plain".into(),
            data: Blob(b"hello".to_vec()),
            available: vec!["text/plain".into(), "text/html".into()],
        }),
    ]);
    call(&mut s, "clip_set", json!({"text": "hello"})).await;
    assert_eq!(
        s.service.requests[0].1,
        Request::ClipSet {
            mime: crate::clipboard::MIME_TEXT.into(),
            data: Blob(b"hello".to_vec())
        }
    );
    let result = call(&mut s, "clip_get", json!({})).await;
    assert_eq!(
        result["content"][1],
        json!({"type": "text", "text": "hello"})
    );
    let result = call(
        &mut s,
        "clip_set",
        json!({"text": "x", "mime": "text/html"}),
    )
    .await;
    assert_eq!(result["isError"], true);
}

#[tokio::test]
async fn list_targets_reports_unreachable_targets_with_their_error() {
    let mut s = server([Ok(Response::error(ErrorCode::Unreachable, "timed out"))]);
    s.service.targets = vec![AgentTarget {
        target: "3".into(),
        name: "megapc".into(),
        fingerprint: None,
    }];
    let result = call(&mut s, "list_targets", json!({})).await;
    let listed: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(listed[0]["name"], "megapc");
    assert_eq!(listed[0]["error"]["code"], "unreachable");
    assert_eq!(s.service.requests[0], ("3".into(), Request::Hello));
}

#[tokio::test]
async fn unknown_tools_and_methods_are_protocol_errors() {
    let mut s = server([]);
    let answer = rpc(
        &mut s,
        "tools/call",
        json!({"name": "ui_tree", "arguments": {}}),
    )
    .await;
    assert_eq!(answer["error"]["code"], INVALID_PARAMS);
    let answer = rpc(&mut s, "resources/list", json!({})).await;
    assert_eq!(answer["error"]["code"], METHOD_NOT_FOUND);
    let answer = rpc(&mut s, "ping", Value::Null).await;
    assert_eq!(answer["result"], json!({}));
}

#[tokio::test]
async fn notifications_get_no_answer() {
    let mut s = server([]);
    let line = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    assert_eq!(s.handle(line).await, None);
}

#[tokio::test]
async fn a_batch_gets_the_answers_of_its_requests() {
    let mut s = server([]);
    let line = br#"[{"jsonrpc":"2.0","method":"notifications/initialized"},
        {"jsonrpc":"2.0","id":1,"method":"ping"},
        {"jsonrpc":"2.0","id":2,"method":"nope"}, 5]"#;
    let answer = s.handle(line).await.expect("requests are answered");
    let answers = answer.as_array().unwrap();
    assert_eq!(answers.len(), 3);
    assert_eq!(answers[0], json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
    assert_eq!(answers[1]["error"]["code"], METHOD_NOT_FOUND);
    assert_eq!(answers[2]["error"]["code"], INVALID_REQUEST);
    let notifications = br#"[{"jsonrpc":"2.0","method":"notifications/initialized"}]"#;
    assert_eq!(s.handle(notifications).await, None);
    let empty = s.handle(b"[]").await.expect("answered");
    assert_eq!(empty["error"]["code"], INVALID_REQUEST);
}

#[tokio::test]
async fn a_malformed_line_is_answered_and_the_server_keeps_running() {
    let mut s = server([]);
    let input: &[u8] =
        b"{not json\n\xff\xfe\n\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";
    let mut output = Vec::new();
    serve(&mut s, input, &mut output).await.unwrap();
    let lines: Vec<Value> = output
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_slice(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["error"]["code"], PARSE_ERROR);
    assert_eq!(lines[0]["id"], Value::Null);
    assert_eq!(lines[1]["error"]["code"], PARSE_ERROR);
    assert_eq!(lines[2], json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
}
