use std::collections::HashMap;

use futures_util::{future, pin_mut, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

#[tokio::main]
async fn main() {
    let url = url::Url::parse("ws:127.0.0.1/chat").unwrap();

    let (stdin_tx, stdin_rx) = futures_channel::mpsc::unbounded();
    tokio::spawn(read_stdin(stdin_tx));

    let (ws_stream, _) = connect_async(url).await.expect("Failed to connect");

    let (write, read) = ws_stream.split();

    //发送消息
    let stdin_to_ws = stdin_rx.map(|a| Ok(a)).forward(write);

    //回显消息
    let ws_to_stdout = read.for_each(|message| async {
        let res = match message {
            Ok(s) => s,
            Err(_) => {
                panic!("掉线了");
            }
        };
        let data = res.into_data();
        println!("{}", std::str::from_utf8(&data).unwrap());
        tokio::io::stdout().write_all(&data).await.unwrap();
    });
    //
    pin_mut!(stdin_to_ws, ws_to_stdout);
    future::select(stdin_to_ws, ws_to_stdout).await;
}

// Our helper method which will read data from stdin and send it along the
// sender provided.
async fn read_stdin(tx: futures_channel::mpsc::UnboundedSender<Message>) {
    let mut stdin = tokio::io::stdin();
    loop {
        let mut buf = vec![0; 1024];
        let n = match stdin.read(&mut buf).await {
            Err(_) | Ok(0) => break,
            Ok(n) => n,
        };
        buf.truncate(n);
        tx.unbounded_send(Message::binary(buf)).unwrap();
    }
}
