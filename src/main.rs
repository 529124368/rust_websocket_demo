use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::mpsc::Sender;

use futures_util::{SinkExt, StreamExt, TryFutureExt};
use tokio::sync::{mpsc, RwLock};
use warp::ws::{Message, WebSocket};
use warp::Filter;

/// Our global unique user id counter.
static NEXT_USER_ID: AtomicUsize = AtomicUsize::new(1);

struct States {
    m1: mpsc::UnboundedSender<Message>,
}

impl States {
    fn new(m1: mpsc::UnboundedSender<Message>) -> Self {
        Self { m1 }
    }
}

type Users = Arc<RwLock<HashMap<usize, States>>>;

type R = Arc<RwLock<Sender<bool>>>;

#[tokio::main]
async fn main() {
    // Keep track of all connected users, key is usize, value
    // is a websocket sender.
    let users = Users::default();
    // Turn our "state" into a new Filter...
    let users = warp::any().map(move || users.clone());

    // GET /chat -> websocket upgrade
    let chat = warp::path("chat")
        // The `ws()` filter will prepare Websocket handshake...
        .and(warp::ws())
        .and(users)
        .map(|ws: warp::ws::Ws, users| {
            // This will call our function if the handshake succeeds.
            ws.on_upgrade(move |socket| user_connected(socket, users))
        });

    // GET / -> index html
    let index = warp::path::end().map(|| warp::reply::html(INDEX_HTML));

    let routes = index.or(chat);

    warp::serve(routes).run(([127, 0, 0, 1], 80)).await;
}

async fn user_connected(ws: WebSocket, users: Users) {
    // Use a counter to assign a new unique ID for this user.
    let my_id = NEXT_USER_ID.fetch_add(1, Ordering::Relaxed);
    eprintln!("new chat user: {}", my_id);
    // Split the socket into a sender and receive of messages.
    let (mut user_ws_tx, mut user_ws_rx) = ws.split();

    //心跳检测管道
    let (alive_send, mut alive_read) = mpsc::channel(1);
    let alive_send = Arc::new(RwLock::new(alive_send));
    let alive_send1 = alive_send.clone();
    //断线通知管道
    let (exit_send, mut exit_read) = mpsc::channel(1);
    let exit_send = Arc::new(RwLock::new(exit_send));
    let exit_send1 = exit_send.clone();
    //消息管道
    let (tx, mut rx) = mpsc::unbounded_channel();
    //登录user table
    users.write().await.insert(my_id, States::new(tx));

    //
    let handle1 = tokio::task::spawn(async move {
        while let Some(message) = rx.recv().await {
            user_ws_tx
                .send(message)
                .unwrap_or_else(|e| {
                    eprintln!("websocket send error: {}", e);
                })
                .await;
        }
    });
    let users1 = users.clone();
    let handle2 = tokio::task::spawn(async move {
        loop {
            tokio::select! {
                 _ =async{alive_read.recv().await} => {
                    eprintln!("live");
                },
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(30)) => {
                    eprintln!("超过30s强制踢下线");
                    let _=exit_send.write().await.send(true).await;
                    //广播消息
                    broadcast_message(my_id, Message::text(format!("<User#{}>:被踢下线了", my_id)),&users).await;
                    user_disconnected(my_id, &users,&alive_send).await;
                    return;
                },
            }
        }
    });

    let handle = tokio::task::spawn(async move {
        while let Some(result) = user_ws_rx.next().await {
            alive_send1
                .write()
                .await
                .send(true)
                .unwrap_or_else(|e| {
                    eprintln!("send error: {}", e);
                })
                .await;
            println!("{}@接受消息", my_id);
            let msg = match result {
                Ok(msg) => msg,
                Err(e) => {
                    eprintln!("websocket error(uid={}): {}", my_id, e);
                    break;
                }
            };
            //广播消息
            broadcast_message(my_id, msg, &users1).await;
        }
        let _ = exit_send1.write().await.send(true).await;
        user_disconnected(my_id, &users1, &alive_send1).await;
    });

    //监听消息
    while let Some(_) = exit_read.recv().await {
        println!("ws链接关闭");
        exit_read.close();
        //强制断开链接
        handle.abort();
        //println!("{}", handle.await.unwrap_err().is_cancelled());
        handle1.abort();
        handle2.abort();
        break;
    }
}

async fn broadcast_message(my_id: usize, msg: Message, users: &Users) {
    // Skip any non-Text messages...
    let msg = if let Ok(s) = msg.to_str() {
        s
    } else {
        return;
    };

    let new_msg = format!("<User#{}>: {}", my_id, msg);

    // New message from this user, send it to everyone else (except same uid)...
    for (&uid, tx) in users.read().await.iter() {
        if my_id != uid {
            if let Err(_disconnected) = tx.m1.send(Message::text(new_msg.clone())) {
                // The tx is disconnected, our `user_disconnected` code
                // should be happening in another task, nothing more to
                // do here.
            }
        }
    }
}

async fn user_disconnected(my_id: usize, users: &Users, r: &R) {
    eprintln!("good bye user: {}", my_id);
    let _ = r.read().await.clone();
    // Stream closed up, so remove from the user list
    users.write().await.remove(&my_id);
}

static INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
    <head>
        <title>聊天室</title>
    </head>
    <body>
        <h1>聊天室</h1>
        <div id="chat">
            <p><em>Connecting...</em></p>
        </div>
        <input type="text" id="text" />
        <button type="button" id="send">发送消息</button>
        <script type="text/javascript">
        const chat = document.getElementById('chat');
        const text = document.getElementById('text');
        const uri = 'ws://' + location.host + '/chat';
        const ws = new WebSocket(uri);
        function message(data) {
            const line = document.createElement('p');
            line.innerText = data;
            chat.appendChild(line);
        }
        ws.onopen = function() {
            chat.innerHTML = '<p><em>Connected!</em></p>';
        };
        ws.onmessage = function(msg) {
            message(msg.data);
        };
        ws.onclose = function() {
            chat.getElementsByTagName('em')[0].innerText = 'Disconnected!';
        };
        send.onclick = function() {
            const msg = text.value;
            ws.send(msg);
            text.value = '';
            message('<You>: ' + msg);
        };
        </script>
    </body>
</html>
"#;
