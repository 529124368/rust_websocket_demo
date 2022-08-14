use msg_base::Vector2;
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
mod msg_base;
/// Our global unique user id counter.
static NEXT_USER_ID: AtomicUsize = AtomicUsize::new(1);

struct States {
    //管道
    m1: mpsc::UnboundedSender<Message>,
    //玩家位置信息
    position: Vector2,
}

impl States {
    fn new(m1: mpsc::UnboundedSender<Message>) -> Self {
        Self {
            m1,
            position: Vector2 { x: 0.0, y: 0.0 },
        }
    }
}

type Users = Arc<RwLock<HashMap<usize, States>>>;

type R = Arc<RwLock<Sender<bool>>>;

#[tokio::main]
async fn main() {
    let users = Users::default();
    // Turn our "state" into a new Filter...
    let users = warp::any().map(move || users.clone());

    // GET /chat -> websocket upgrade
    let routes = warp::path("chat")
        .and(warp::ws())
        .and(users)
        .map(|ws: warp::ws::Ws, users| {
            // This will call our function if the handshake succeeds.
            ws.on_upgrade(move |socket| user_connected(socket, users))
        });

    // GET / -> index html
    // let index = warp::path::end().map(|| warp::reply::html(INDEX_HTML));

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
    let users_new = users.clone();
    //广播上线消息
    tokio::task::spawn(async move {
        let msg = msg_base::MsgBase::new(0, my_id as u32, format!("玩家#{}:上线了", my_id));
        let msg = serde_json::to_string(&msg).unwrap();
        broadcast_message(my_id, Message::text(msg), &users_new).await;
    });

    //往客户端发送消息
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
    //心跳检测
    let handle2 = tokio::task::spawn(async move {
        loop {
            tokio::select! {
                 _ =async{alive_read.recv().await} => {
                },
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(60)) => {
                    eprintln!("超过60s强制踢下线");
                    let _=exit_send.write().await.send(true).await;
                    user_disconnected(my_id, &users,&alive_send).await;
                    return;
                },
            }
        }
    });

    //接受客户端消息
    let handle = tokio::task::spawn(async move {
        while let Some(result) = user_ws_rx.next().await {
            //心跳监测
            alive_send1
                .write()
                .await
                .send(true)
                .unwrap_or_else(|e| {
                    eprintln!("send error: {}", e);
                })
                .await;
            match result {
                Ok(msg) => {
                    //广播消息
                    broadcast_message(my_id, msg, &users1).await;
                }
                Err(e) => {
                    eprintln!("websocket error(uid={}): {}", my_id, e);
                    break;
                }
            };
        }
        let _ = exit_send1.write().await.send(true).await;
        user_disconnected(my_id, &users1, &alive_send1).await;
    });

    //监听消息
    if exit_read.recv().await.is_some() {
        //强制断开链接
        exit_read.close();
        handle.abort();
        handle1.abort();
        handle2.abort();
    }
}

//广播消息
async fn broadcast_message(my_id: usize, msg: Message, users: &Users) {
    // Skip any non-Text messages...
    let msg = if let Ok(s) = std::str::from_utf8(msg.as_bytes()) {
        s
    } else {
        return;
    };
    let p: msg_base::MsgBase = serde_json::from_str(msg).unwrap();
    //心跳检测消息跳过转发
    //3 -> 心跳检测消息
    if p.mes_type != 3 {
        let mut _new_msg = String::new();
        if p.mes_type == 0 {
            let msg = msg_base::MsgBase::new(
                0,
                my_id as u32,
                format!(
                    "系统消息:{}\n当前在线人数为:{}",
                    p.message,
                    users.read().await.len()
                ),
            );
            _new_msg = serde_json::to_string(&msg).unwrap();
        } else if p.mes_type == 1 {
            let msg = msg_base::MsgBase::new(
                1,
                my_id as u32,
                format!("玩家#{} say:{}", my_id, p.message),
            );
            _new_msg = serde_json::to_string(&msg).unwrap();
        } else {
            if p.message.contains("has_who") {
                let mut name_list = "".to_string();
                for (&uid, _) in users.read().await.iter() {
                    if my_id != uid {
                        name_list += &(uid.to_string() + "|");
                    }
                }
                let msg =
                    msg_base::MsgBase::new(2, my_id as u32, "has_who:".to_string() + &name_list);
                _new_msg = serde_json::to_string(&msg).unwrap();
                //单独发送消息 不再群发
                users
                    .read()
                    .await
                    .get(&my_id)
                    .unwrap()
                    .m1
                    .send(Message::text(_new_msg))
                    .unwrap();
                return;
            } else {
                let mut msg = msg_base::MsgBase::new(2, my_id as u32, p.message);
                msg.position = p.position;
                msg.direaction = p.direaction;
                _new_msg = serde_json::to_string(&msg).unwrap();
                //更新坐标
                let mut dd = users.write().await;
                let d = dd.get_mut(&my_id).unwrap();
                d.position = msg.position;
            }
        }

        //群发消息
        for (&uid, tx) in users.read().await.iter() {
            if my_id != uid {
                let _ = tx.m1.send(Message::text(_new_msg.clone()));
            }
        }
    }
}

//玩家下线
async fn user_disconnected(my_id: usize, users: &Users, r: &R) {
    eprintln!("good bye user: {}", my_id);
    let msg = msg_base::MsgBase::new(0, my_id as u32, format!("玩家#{}:下线了", my_id));
    let msg = serde_json::to_string(&msg).unwrap();
    let _ = r.read().await.clone();
    // Stream closed up, so remove from the user list
    users.write().await.remove(&my_id);
    //广播消息
    broadcast_message(my_id, Message::text(msg), users).await;
}
