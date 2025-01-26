use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{RwLock, Mutex};
use tokio_tungstenite::tungstenite::Message;
use futures_util::{StreamExt, SinkExt};
use tokio_stream::wrappers::UnboundedReceiverStream;
use serde::{Serialize, Deserialize};
use sqlx::FromRow;
use sqlx::sqlite::SqlitePool;

type PeerMap = Arc<RwLock<HashMap<String, tokio::sync::mpsc::UnboundedSender<Message>>>>;
type CanvasState = Arc<RwLock<Vec<DrawAction>>>;

#[derive(Serialize, Deserialize, Clone, FromRow)]
struct DrawAction {
    tool: String,
    color: String,
    line_width: i32,
    start_x: f64,
    start_y: f64,
    end_x: f64,
    end_y: f64,
}

async fn init_db(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS canvas_actions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tool TEXT NOT NULL,
            color TEXT NOT NULL,
            line_width INTEGER NOT NULL,
            start_x REAL NOT NULL,
            start_y REAL NOT NULL,
            end_x REAL NOT NULL,
            end_y REAL NOT NULL,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        )"
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn load_canvas_state(pool: &SqlitePool) -> Result<Vec<DrawAction>, sqlx::Error> {
    sqlx::query_as::<_, DrawAction>(
        "SELECT tool, color, line_width, start_x, start_y, end_x, end_y FROM canvas_actions ORDER BY id ASC"
    )
    .fetch_all(pool)
    .await
}

async fn handle_connection(peer_map: PeerMap, pool: SqlitePool, raw_stream: TcpStream, addr: String) {
    println!("Incoming TCP connection from: {}", addr);

    let ws_stream = tokio_tungstenite::accept_async(raw_stream)
        .await
        .expect("Error during WebSocket handshake");
    println!("WebSocket connection established: {}", addr);

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    peer_map.write().await.insert(addr.clone(), tx.clone());

    let current_state = load_canvas_state(&pool).await.unwrap_or_default();
    if !current_state.is_empty() {
        let state_msg = serde_json::to_string(&current_state).unwrap();
        tx.send(Message::Text(state_msg)).unwrap_or_default();
    }

    let (outgoing, incoming) = ws_stream.split();
    let outgoing = Arc::new(Mutex::new(outgoing));

    let broadcast_incoming = {
        let pool = pool.clone();
        let tx = tx.clone();
        let peer_map = peer_map.clone();
        let addr = addr.clone();
        
        incoming.for_each(move |msg| {
            let pool = pool.clone();
            let tx = tx.clone();
            let peer_map = peer_map.clone();
            let addr = addr.clone();
            
            async move {
                if let Ok(msg) = msg {
                    if let Ok(data) = serde_json::from_str::<serde_json::Value>(msg.to_string().as_str()) {
                        match data.get("type").and_then(|t| t.as_str()) {
                            Some("requestState") => {
                                let state = load_canvas_state(&pool).await.unwrap_or_default();
                                if !state.is_empty() {
                                    let state_msg = serde_json::to_string(&state).unwrap();
                                    tx.send(Message::Text(state_msg)).unwrap_or_default();
                                }
                            }
                            _ => if let Ok(action) = serde_json::from_value::<DrawAction>(data) {
                                sqlx::query(
                                    "INSERT INTO canvas_actions (tool, color, line_width, start_x, start_y, end_x, end_y) 
                                     VALUES (?, ?, ?, ?, ?, ?, ?)"
                                )
                                .bind(&action.tool)
                                .bind(&action.color)
                                .bind(action.line_width)
                                .bind(action.start_x)
                                .bind(action.start_y)
                                .bind(action.end_x)
                                .bind(action.end_y)
                                .execute(&pool)
                                .await
                                .unwrap_or_default();
                                
                                let peers = peer_map.read().await;
                                for (peer_addr, tx) in peers.iter() {
                                    if peer_addr != &addr {
                                        tx.send(msg.clone()).unwrap_or_default();
                                    }
                                }
                            }
                        }
                    }
                }
            }
        })
    };

    let receive_from_others = {
        let outgoing = outgoing.clone();
        UnboundedReceiverStream::new(rx).for_each(move |msg| {
            let outgoing = outgoing.clone();
            async move {
                let mut outgoing = outgoing.lock().await;
                outgoing.send(msg).await.unwrap_or_default();
            }
        })
    };

    tokio::select! {
        _ = broadcast_incoming => {},
        _ = receive_from_others => {},
    };

    peer_map.write().await.remove(&addr);
}

#[tokio::main]
async fn main() {
    let pool = SqlitePool::connect("sqlite:canvas.db")
        .await
        .expect("Failed to connect to database");
    
    init_db(&pool).await.expect("Failed to initialize database");

    let ports = [8080, 8081, 8082, 3000, 3001];
    let mut server_addr = None;

    for port in ports {
        let addr = format!("127.0.0.1:{}", port);
        match TcpListener::bind(&addr).await {
            Ok(listener) => {
                server_addr = Some((addr, listener));
                break;
            }
            Err(_) => continue,
        }
    }

    let (addr, listener) = server_addr.expect("No available ports found");
    println!("Listening on: {}", addr);

    let peer_map = PeerMap::default();

    while let Ok((stream, _)) = listener.accept().await {
        let peer_addr = stream.peer_addr().unwrap().to_string();
        let peer_map = peer_map.clone();
        let pool = pool.clone();

        tokio::spawn(async move {
            handle_connection(peer_map, pool, stream, peer_addr).await;
        });
    }
}
