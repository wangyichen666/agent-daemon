use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::client::DaemonClient;
use crate::daemon::protocol::{
    JsonRpcResponse, MAX_FRAME_BYTES, RequestId, ServerFrame, decode_request, encode_frame,
    server_frame_request_id,
};

pub async fn run_stdio_adapter(client: DaemonClient) -> Result<()> {
    let mut input = BufReader::new(tokio::io::stdin());
    let (output, mut output_receiver) = mpsc::unbounded_channel::<ServerFrame>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(frame) = output_receiver.recv().await {
            let encoded = match encode_frame(&frame) {
                Ok(encoded) => encoded,
                Err(error) => encode_frame(&ServerFrame::Response(JsonRpcResponse::failure(
                    server_frame_request_id(&frame).clone(),
                    -32002,
                    error.to_string(),
                )))
                .context("编码编辑器协议超限响应失败")?,
            };
            stdout
                .write_all(&encoded)
                .await
                .context("写入编辑器 stdout 失败")?;
            stdout.flush().await.context("刷新编辑器 stdout 失败")?;
        }
        Ok::<(), anyhow::Error>(())
    });
    let mut jobs = JoinSet::new();

    loop {
        let mut line = Vec::new();
        let read = input
            .read_until(b'\n', &mut line)
            .await
            .context("读取编辑器 stdin 失败")?;
        if read == 0 {
            break;
        }
        let request = match decode_request(&line) {
            Ok(request) => request,
            Err(error) => {
                let _ = output.send(ServerFrame::Response(JsonRpcResponse::failure(
                    RequestId::String("protocol".to_owned()),
                    if line.len() > MAX_FRAME_BYTES {
                        -32002
                    } else {
                        -32700
                    },
                    error.to_string(),
                )));
                continue;
            }
        };
        let client = client.clone();
        let output = output.clone();
        jobs.spawn(async move {
            let request_id = request.id.clone();
            let stream = client
                .request_with_id(request.id, &request.method, request.params)
                .await;
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(error) => {
                    let _ = output.send(ServerFrame::Response(JsonRpcResponse::failure(
                        request_id,
                        -32000,
                        format!("{error:#}"),
                    )));
                    return;
                }
            };
            while let Some(frame) = stream.next().await {
                let terminal = matches!(frame, ServerFrame::Response(_));
                if output.send(frame).is_err() || terminal {
                    break;
                }
            }
        });
    }

    while jobs.join_next().await.is_some() {}
    drop(output);
    writer.await.context("编辑器 stdout 任务异常终止")??;
    Ok(())
}
