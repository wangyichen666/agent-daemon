use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use crate::tool_calls::collect_provider_response;

#[tokio::test]
async fn openai_mock_closes_tool_result_round_trip() {
    let responses = vec![
        MockHttpResponse::sse(concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"echo\",\"arguments\":\"{\\\"value\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"ok\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n"
        )),
        MockHttpResponse::sse(concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"完成\"}}]}\n\n",
            "data: [DONE]\n\n"
        )),
    ];
    let (base_url, requests) = start_mock_http(responses).await;
    let provider = OpenAiProvider::new("key".to_owned(), base_url, "model".to_owned());
    assert_wire_round_trip(&provider, requests, "\"tool_call_id\":\"call_1\"").await;
}

#[tokio::test]
async fn anthropic_mock_closes_tool_result_round_trip() {
    let responses = vec![
        MockHttpResponse::sse(concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"echo\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"value\\\":\\\"ok\\\"}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        )),
        MockHttpResponse::sse(concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"完成\"}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        )),
    ];
    let (base_url, requests) = start_mock_http(responses).await;
    let provider = AnthropicProvider::new("key".to_owned(), base_url, "model".to_owned());
    assert_wire_round_trip(&provider, requests, "\"tool_use_id\":\"toolu_1\"").await;
}

#[tokio::test]
async fn ollama_mock_closes_tool_result_round_trip() {
    let responses = vec![
        MockHttpResponse::ndjson(concat!(
            "{\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"echo\",\"arguments\":{\"value\":\"ok\"}}}]},\"done\":false}\n",
            "{\"done\":true,\"prompt_eval_count\":10,\"eval_count\":2}\n"
        )),
        MockHttpResponse::ndjson(concat!(
            "{\"message\":{\"role\":\"assistant\",\"content\":\"完成\"},\"done\":false}\n",
            "{\"done\":true,\"prompt_eval_count\":12,\"eval_count\":1}\n"
        )),
    ];
    let (base_url, requests) = start_mock_http(responses).await;
    let provider = OllamaProvider::new(base_url, "model".to_owned(), true);
    assert_wire_round_trip(&provider, requests, "\"role\":\"tool\"").await;
}

async fn assert_wire_round_trip(
    provider: &dyn Provider,
    mut requests: tokio::sync::mpsc::UnboundedReceiver<String>,
    expected_result_shape: &str,
) {
    let tools = [ToolSpec {
        name: "echo".to_owned(),
        description: "echo".to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"]
        }),
    }];
    let mut history = vec![Message::text(Role::User, "调用 echo")];
    let Response::ToolCalls(calls) = collect_provider_response(provider, &history, &tools)
        .await
        .expect("第一次 mock 请求应成功")
    else {
        panic!("第一次 mock 响应应为工具调用");
    };
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].arguments["value"], "ok");
    history.push(Message::assistant_tool_calls(calls.clone()));
    history.push(Message::tool_result(&calls[0], "echo: ok"));
    let response = collect_provider_response(provider, &history, &tools)
        .await
        .expect("第二次 mock 请求应成功");
    assert_eq!(response, Response::Text("完成".to_owned()));
    let _first = requests.recv().await.expect("缺少第一次请求");
    let second = requests.recv().await.expect("缺少第二次请求");
    assert!(
        second.contains(expected_result_shape),
        "工具结果未按协议回填：{second}"
    );
}

struct MockHttpResponse {
    content_type: &'static str,
    body: String,
}

impl MockHttpResponse {
    fn sse(body: impl Into<String>) -> Self {
        Self {
            content_type: "text/event-stream",
            body: body.into(),
        }
    }

    fn ndjson(body: impl Into<String>) -> Self {
        Self {
            content_type: "application/x-ndjson",
            body: body.into(),
        }
    }
}

async fn start_mock_http(
    responses: Vec<MockHttpResponse>,
) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定 mock HTTP 失败");
    let address = listener.local_addr().expect("读取 mock 地址失败");
    let (requests, receiver) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        for response in responses {
            let (mut stream, _) = listener.accept().await.expect("接受 mock 请求失败");
            let request = read_http_request(&mut stream)
                .await
                .expect("读取 mock 请求失败");
            requests.send(request).expect("保存 mock 请求失败");
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.content_type,
                response.body.len()
            );
            stream
                .write_all(head.as_bytes())
                .await
                .expect("写 mock 响应头失败");
            stream
                .write_all(response.body.as_bytes())
                .await
                .expect("写 mock 响应体失败");
        }
    });
    (format!("http://{address}"), receiver)
}

async fn read_http_request(stream: &mut tokio::net::TcpStream) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = find_bytes(&bytes, b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if bytes.len() >= header_end + 4 + content_length {
                break;
            }
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
