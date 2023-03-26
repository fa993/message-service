use std::fmt::Debug;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWrite;
use tokio::sync::broadcast::{Receiver, Sender};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::{io::AsyncRead, net::TcpListener};

use tokio::time::sleep;
use tokio_tungstenite::{connect_async, WebSocketStream};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LocatedMessage {
    pub body: String,
    source: String,
}

pub async fn make_dist() -> (Sender<String>, UnboundedReceiver<String>) {
    let (outer_tx, _) = tokio::sync::broadcast::channel(100);
    let (inneruse_tx_s, outer_rc) = tokio::sync::mpsc::unbounded_channel();

    let sp: &'static str = option_env!("SELF_IP").unwrap_or("0.0.0.0:3456");
    let _ = dotenv::dotenv();

    option_env!("TO_CONNECT")
        .unwrap_or("")
        .split(',')
        .for_each(|connect_addr| {
            if connect_addr.is_empty() {
                return;
            }
            println!("Trying to connect to: {connect_addr}");
            let inneruse_rx = outer_tx.subscribe();
            let inneruse_tx = inneruse_tx_s.clone();
            tokio::spawn(async move {
                let ws_stream;
                loop {
                    sleep(Duration::from_secs(5)).await;
                    let yyyy = connect_async(connect_addr).await;
                    if let Ok(t) = yyyy {
                        (ws_stream, _) = t;
                        break;
                    } else {
                        println!("{:?}", yyyy);
                    }
                    println!("Failed to connect, trying again")
                }
                println!("WebSocket handshake has been successfully completed");

                // let (mut write, read) = ws_stream.split();
                // let mut snd_tsk = tokio::spawn(async move {
                //     while let Ok(msg) = inneruse_rx.recv().await {
                //         let _ = write.send(tokio_tungstenite::tungstenite::Message::Text(
                //             serde_json::to_string(&LocatedMessage {
                //                 body: msg,
                //                 source: sp.to_string(),
                //             })
                //             .unwrap(),
                //         ));
                //     }
                // });

                // let mut recv_task = tokio::spawn(async move {
                //     read.for_each(|message| async {
                //         let data: LocatedMessage =
                //             serde_json::from_str(message.unwrap().into_text().unwrap().as_str())
                //                 .unwrap();
                //         if data.source == sp {
                //             let _ = inneruse_tx.send(data.body);
                //         }
                //     })
                //     .await
                // });

                // tokio::select! {
                //     _ = (&mut snd_tsk) => recv_task.abort(),
                //     _ = (&mut recv_task) => snd_tsk.abort(),
                // };
                handle_stream(ws_stream, sp, inneruse_rx, inneruse_tx).await;
            });
        });

    let second_outer = outer_tx.clone();

    //also spawn listener
    tokio::spawn(async move {
        let addr = sp;

        // Create the event loop and TCP listener we'll accept connections on.
        let try_socket = TcpListener::bind(&addr).await;
        let listener = try_socket.expect("Failed to bind");
        println!("Listening on: {}", addr);

        // Let's spawn the handling of each connection in a separate task.
        while let Ok((stream, addr)) = listener.accept().await {
            let inneruse_rx = second_outer.subscribe();
            let inneruse_tx = inneruse_tx_s.clone();
            tokio::spawn(async move {
                println!("Incoming TCP connection from: {}", addr);

                let ws_stream = tokio_tungstenite::accept_async(stream).await.unwrap();

                // let (mut write, read) = ws_stream.split();

                // let mut snd_tsk = tokio::spawn(async move {
                //     while let Ok(msg) = inneruse_rx.recv().await {
                //         let _ = write.send(tokio_tungstenite::tungstenite::Message::Text(
                //             serde_json::to_string(&LocatedMessage {
                //                 body: msg,
                //                 source: sp.to_string(),
                //             })
                //             .unwrap(),
                //         ));
                //     }
                // });

                // let mut recv_task = tokio::spawn(async move {
                //     read.for_each(|message| async {
                //         let data: LocatedMessage =
                //             serde_json::from_str(message.unwrap().into_text().unwrap().as_str())
                //                 .unwrap();
                //         if data.source == sp {
                //             let _ = inneruse_tx.send(data.body);
                //         }
                //     })
                //     .await
                // });

                // tokio::select! {
                //     _ = (&mut snd_tsk) => recv_task.abort(),
                //     _ = (&mut recv_task) => snd_tsk.abort(),
                // };
                handle_stream(ws_stream, sp, inneruse_rx, inneruse_tx).await;

                // println!("WebSocket connection established: {addr}");
            });
        }
    });

    return (outer_tx, outer_rc);
}

//TODO figure out if static lifetime is actually non-performant
async fn handle_stream<S>(
    ws_stream: WebSocketStream<S>,
    sp: &'static str,
    mut inneruse_rx: Receiver<String>,
    inneruse_tx: UnboundedSender<String>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut write, read) = ws_stream.split();
    let mut snd_tsk = tokio::spawn(async move {
        while let Ok(msg) = inneruse_rx.recv().await {
            let rt = write.send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&LocatedMessage {
                    body: msg,
                    source: sp.to_string(),
                })
                .unwrap(),
            )).await;
            println!("{:?}", rt);
        }
    });

    let mut recv_task = tokio::spawn(async move {
        read.for_each(|message| async {
            let data: LocatedMessage =
                serde_json::from_str(message.unwrap().into_text().unwrap().as_str()).unwrap();
            if data.source != sp {
                let _ = inneruse_tx.send(data.body);
            }
        })
        .await
    });

    println!("WebSocket connection established");

    tokio::select! {
        _ = (&mut snd_tsk) => recv_task.abort(),
        _ = (&mut recv_task) => snd_tsk.abort(),
    };

    println!("Running away");
}
