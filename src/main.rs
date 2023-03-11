use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::Arc,
};

use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, State, WebSocketUpgrade,
    },
    http::{Request, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router, ServiceExt,
};
use futures::{SinkExt, StreamExt};
use rand::random;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tower::{Layer, ServiceBuilder};
use tracing_subscriber::{prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt};

enum WriterMessage<T: Send + Sync> {
    InsertIfNotExists(T),
}

pub fn single_writer_set() -> (mpsc::UnboundedSender<String>, async_channel::Receiver<bool>) {
    let mut set = HashSet::new();
    let r = async_channel::unbounded::<bool>();
    let mut t = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(f) = t.1.recv().await {
            if set.contains(f.as_str()) {
                r.0.send(false).await;
            } else {
                set.insert(f);
                r.0.send(true).await;
            }
        }
    });
    (t.0, r.1)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
enum ChatMessage {
    System { body: String },
    User { sender: String, body: String },
}

struct ChatState {
    // usernames: (UnboundedSender<String>, UnboundedReceiver<bool>),
    usernames: Mutex<HashSet<String>>,
    broadcast: broadcast::Sender<ChatMessage>,
}

impl ChatState {
    pub fn new() -> Self {
        Self {
            broadcast: broadcast::channel(100_000).0,
            usernames: Default::default(),
        }
    }
}

async fn chat_room_adder(
    ws: WebSocketUpgrade,
    Path((channel, name)): Path<(u32, String)>,
    State(state): State<Arc<RwLock<HashMap<u32, ChatState>>>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_add(socket, channel, name, state))
}

async fn handle_add(
    ws: WebSocket,
    channel: u32,
    name: String,
    state: Arc<RwLock<HashMap<u32, ChatState>>>,
) {
    //extract into layer
    let (mut sender, mut receiver) = ws.split();

    let _ = sender
        .send(Message::Text(
            serde_json::to_string(&ChatMessage::System {
                body: format!("Joined Channel {channel}"),
            })
            .unwrap(),
        ))
        .await;

    let mut rx = state.read().await[&channel].broadcast.subscribe();

    let msg = ChatMessage::System {
        body: format!("User {name} joined"),
    };

    state.read().await[&channel].broadcast.send(msg).unwrap();

    let mut send_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            // In any websocket error, break loop.
            if sender
                .send(Message::Text(serde_json::to_string(&msg).unwrap()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let tx = state.read().await[&channel].broadcast.clone();
    let usname = name.clone();

    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = receiver.next().await {
            // Add username before message.
            let _ = tx.send(ChatMessage::User {
                sender: usname.clone(),
                body: text,
            });
        }
    });

    tokio::select! {
        _ = (&mut send_task) => recv_task.abort(),
        _ = (&mut recv_task) => send_task.abort(),
    };

    let msg = ChatMessage::System {
        body: format!("User {name} left"),
    };

    state.read().await[&channel].broadcast.send(msg).unwrap();

    state.read().await[&channel]
        .usernames
        .lock()
        .await
        .remove(name.as_str());
}

async fn add_channel_layer<B>(
    Path((channel, _)): Path<(u32, String)>,
    State(state): State<Arc<RwLock<HashMap<u32, ChatState>>>>,
    request: Request<B>,
    next: Next<B>,
) -> Response {
    if !state.read().await.contains_key(&channel) {
        state.write().await.insert(channel, ChatState::new());
    }
    let response = next.run(request).await;
    response
}

async fn check_username_conflict<B>(
    Path((channel, name)): Path<(u32, String)>,
    State(state): State<Arc<RwLock<HashMap<u32, ChatState>>>>,
    request: Request<B>,
    next: Next<B>,
) -> Result<Response, StatusCode> {
    if !state.read().await[&channel]
        .usernames
        .lock()
        .await
        .insert(name)
    {
        Err(StatusCode::IM_USED)
    } else {
        let response = next.run(request).await;
        Ok(response)
    }
}

async fn generate_new_channel<B>(
    State(state): State<Arc<RwLock<HashMap<u32, ChatState>>>>,
    mut request: Request<B>,
    next: Next<B>,
) -> Response {
    if request.uri().path().split('/').nth(1).unwrap() == "new" {
        let mut k: u32 = random();
        while state.read().await.contains_key(&k)
            && state.read().await[&k].usernames.lock().await.is_empty()
        {
            k = random();
        }
        let r = request.uri_mut();
        let mut by = Uri::builder();
        by = if let Some(ind) = r.scheme() {
            by.scheme(ind.clone())
        } else {
            by
        };
        by = if let Some(ind) = r.authority() {
            by.authority(ind.clone())
        } else {
            by
        };
        *r = by
            .path_and_query(format!("/{}/{}", k, r.path().split('/').last().unwrap()))
            .build()
            .unwrap();
    }
    let response = next.run(request).await;
    response
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "example_chat=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let st = Arc::new(RwLock::new(HashMap::new()));

    let app_ind = Router::new()
        .route("/new/:name", get(chat_room_adder))
        .route("/:channel/:name", get(chat_room_adder))
        .layer(
            ServiceBuilder::new()
                .layer(middleware::from_fn_with_state(
                    st.clone(),
                    add_channel_layer,
                ))
                .layer(middleware::from_fn_with_state(
                    st.clone(),
                    check_username_conflict,
                )),
        )
        .with_state(st.clone());

    let app = middleware::from_fn_with_state(st, generate_new_channel).layer(app_ind);

    println!("Hello, world!");

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    tracing::debug!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}
