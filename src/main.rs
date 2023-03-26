pub mod dist;

use std::{net::SocketAddr, sync::Arc};

use axum::routing::get;
use axum::Router;

use tracing_subscriber::{prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt};

use crate::{dist::make_dist, unmanaged::UnmanagedChats};

// enum WriterMessage<T: Send + Sync> {
//     InsertIfNotExists(T),
// }

// pub fn single_writer_set() -> (mpsc::UnboundedSender<String>, async_channel::Receiver<bool>) {
//     let mut set = HashSet::new();
//     let r = async_channel::unbounded::<bool>();
//     let mut t = mpsc::unbounded_channel::<String>();
//     tokio::spawn(async move {
//         while let Some(f) = t.1.recv().await {
//             if set.contains(f.as_str()) {
//                 r.0.send(false).await;
//             } else {
//                 set.insert(f);
//                 r.0.send(true).await;
//             }
//         }
//     });
//     (t.0, r.1)
// }

// struct ChatState {
//     // usernames: (UnboundedSender<String>, UnboundedReceiver<bool>),
//     usernames: SkipMap<String, String>, // cookie vs username
//     broadcast: broadcast::Sender<UnmanagedChatMessage>,
//     default: Option<broadcast::Sender<UnmanagedChatMessage>>,
// }

// impl ChatState {
//     pub fn new() -> Self {
//         Self {
//             broadcast: broadcast::channel(100_000).0,
//             usernames: Default::default(),
//             default: Default::default(),
//         }
//     }
// }

mod unmanaged {

    use std::{collections::HashMap, sync::Arc};

    use axum::{
        extract::{
            ws::{Message, WebSocket},
            State, WebSocketUpgrade,
        },
        response::IntoResponse,
    };

    use crossbeam_skiplist::SkipMap;
    use futures::{SinkExt, StreamExt};
    use serde::{Deserialize, Serialize};
    use tokio::sync::{
        broadcast::{self, error::SendError, Sender},
        mpsc,
    };
    use uuid::Uuid;

    #[derive(Debug)]
    pub enum RouteError {
        ChannelNotFound,
        BadMessageFormat,
        SendErr(SendError<String>),
    }

    pub struct UnmanagedChats {
        chats: SkipMap<Uuid, broadcast::Sender<String>>,
        global_send: Sender<String>,
    }

    impl UnmanagedChats {
        pub fn new(se: Sender<String>) -> UnmanagedChats {
            UnmanagedChats {
                chats: SkipMap::default(),
                global_send: se,
            }
        }

        pub fn route_message(&self, msg: String) -> Result<(), RouteError> {
            let umc: UnmanagedChatUserMessage =
                serde_json::from_str(&msg).map_err(|_| RouteError::BadMessageFormat)?;
            if let Some(f) = self.chats.get(umc.to()) {
                f.value()
                    .send(msg)
                    .map(|_| ())
                    .map_err(|f| RouteError::SendErr(f))
            } else {
                Err(RouteError::ChannelNotFound)
            }
        }
    }

    #[derive(Clone, Debug, Deserialize, Serialize)]
    enum UnmanagedChatUserMessage {
        Publish { channel: Uuid, body: String },
        Subscribe(Uuid),
        Unsubscribe(Uuid),
    }

    impl UnmanagedChatUserMessage {
        pub fn to(&self) -> &Uuid {
            match &self {
                UnmanagedChatUserMessage::Publish { channel, .. } => channel,
                UnmanagedChatUserMessage::Subscribe(t)
                | UnmanagedChatUserMessage::Unsubscribe(t) => t,
            }
        }
    }

    pub async fn chat_room_adder(
        ws: WebSocketUpgrade,
        State(state): State<Arc<UnmanagedChats>>,
    ) -> impl IntoResponse {
        ws.on_upgrade(move |socket| handle_add(socket, state))
    }

    async fn handle_add(ws: WebSocket, state: Arc<UnmanagedChats>) {
        //extract into layer
        let (mut sender, mut receiver) = ws.split();

        let (txmpc, mut rxmpc) = mpsc::unbounded_channel();

        let mut recv_task_pt2 = tokio::spawn(async move {
            while let Some(t) = rxmpc.recv().await {
                if sender.send(t).await.is_err() {
                    break;
                }
            }
        });

        let mut recv_task = tokio::spawn(async move {
            let mut map = HashMap::new();

            while let Some(Ok(Message::Text(text))) = receiver.next().await {
                // Add username before message.
                println!("{:?}", state.global_send.send(text.clone()));

                let uc: UnmanagedChatUserMessage = serde_json::from_str(&text).unwrap();

                match uc {
                    UnmanagedChatUserMessage::Subscribe(t) => {
                        let rx_tx = state
                            .chats
                            .get_or_insert(t, broadcast::channel(100).0);

                        let mut rx = rx_tx.value().subscribe();
                        let mytxmpc = txmpc.clone();
                        let send_task = tokio::spawn(async move {
                            while let Ok(msg) = rx.recv().await {
                                // In any websocket error, break loop.
                                if mytxmpc
                                    .send(Message::Text(serde_json::to_string(&msg).unwrap()))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        });

                        map.insert(t, send_task);

                        if rx_tx.value().send(text).is_err() {
                            break;
                        }
                    }
                    UnmanagedChatUserMessage::Unsubscribe(t) => {
                        // state.chats
                        if let Some((_, f)) = map.remove_entry(&t) {
                            f.abort();
                        }
                        if let Some(f) = state.chats.get(&t) {
                            if f.value().receiver_count() == 0 {
                                map.remove(&t);
                            }
                            if f.value().send(text).is_err() {
                                break;
                            }
                        }
                    }
                    UnmanagedChatUserMessage::Publish { channel, .. } => {
                        if let Some(t) = state.chats.get(&channel) {
                            if t.value().send(text).is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });

        tokio::select! {
            _ = (&mut recv_task_pt2) => recv_task.abort(),
            _ = (&mut recv_task) => recv_task_pt2.abort(),
        };
    }

    pub fn gen_example_text() {
        println!(
            "{:?}",
            serde_json::to_string(&UnmanagedChatUserMessage::Subscribe(Uuid::nil()))
        );
        println!(
            "{:?}",
            serde_json::to_string(&UnmanagedChatUserMessage::Publish {
                body: "Hey!".to_string(),
                channel: Uuid::nil()
            })
        );
    }
}

// async fn add_channel_layer<B>(
//     Path((channel, _)): Path<(u32, String)>,
//     State(state): State<Arc<UnmanagedChats>>,
//     request: Request<B>,
//     next: Next<B>,
// ) -> Response {
//     state.chats.get_or_insert(channel, ChatState::new());
//     let response = next.run(request).await;
//     response
// }

// async fn check_username_conflict<B>(
//     Path((channel, name)): Path<(u32, String)>,
//     State(state): State<Arc<UnmanagedChats>>,
//     request: Request<B>,
//     next: Next<B>,
// ) -> Result<Response, StatusCode> {
//     if let Some(t) = state.chats.get(&channel) {
//         if !t.value().usernames.lock().await.insert(name) {
//             return Err(StatusCode::IM_USED);
//         }
//     }
//     let response = next.run(request).await;
//     Ok(response)
// }

// async fn generate_new_channel<B>(
//     State(state): State<Arc<UnmanagedChats>>,
//     mut request: Request<B>,
//     next: Next<B>,
// ) -> Response {
//     if request.uri().path().split('/').nth(1).unwrap() == "new" {
//         let mut k: u32 = random();
//         while state.chats.contains_key(&k)
//             && state
//                 .chats
//                 .get(&k)
//                 .unwrap()
//                 .value()
//                 .available
//                 .load(std::sync::atomic::Ordering::SeqCst)
//         {
//             k = random();
//         }
//         let r = request.uri_mut();
//         let mut by = Uri::builder();
//         by = if let Some(ind) = r.scheme() {
//             by.scheme(ind.clone())
//         } else {
//             by
//         };
//         by = if let Some(ind) = r.authority() {
//             by.authority(ind.clone())
//         } else {
//             by
//         };
//         *r = by
//             .path_and_query(format!("/{}/{}", k, r.path().split('/').last().unwrap()))
//             .build()
//             .unwrap();
//     }
//     let response = next.run(request).await;
//     response
// }

// async fn chat_room_creator(
//     Path(name): Path<String>,
//     State(state): State<Arc<UnmanagedChats>>,
// ) -> impl IntoResponse {

// }

#[tokio::main]
async fn main() {
    let _ = dotenv::dotenv();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "message_service=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let (se, mut rx) = make_dist().await;

    let st = Arc::new(UnmanagedChats::new(se));
    let st_2 = st.clone();

    tokio::spawn(async move {
        while let Some(x) = rx.recv().await {
            let r = st_2.route_message(x);
            println!("{:?}", r);
        }
        println!("You're dead");
    });

    let app = Router::new()
        .route("/unmanaged/", get(unmanaged::chat_room_adder))
        // .route("/newchannel", post(chat_room_creator))
        // .route("/:channel/:name", get(chat_room_adder))
        // .layer(
        //     ServiceBuilder::new()
        //         .layer(middleware::from_fn_with_state(
        //             st.clone(),
        //             add_channel_layer,
        //         ))
        //         .layer(middleware::from_fn_with_state(
        //             st.clone(),
        //             check_username_conflict,
        //         )),
        // )
        .with_state(st.clone());

    // let app = middleware::from_fn_with_state(st, generate_new_channel).layer(app_ind);

    unmanaged::gen_example_text();

    let addr = SocketAddr::from((
        [0, 0, 0, 0],
        option_env!("PORT")
            .and_then(|f| f.parse().ok())
            .unwrap_or(3000),
    ));
    tracing::debug!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}
