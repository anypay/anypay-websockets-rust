use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{accept_async, tungstenite::Message as WsMessage};
use futures::{StreamExt, SinkExt};
use uuid::Uuid;
use serde_json::json;

use crate::event_dispatcher::EventDispatcher;
use crate::session::Session;
use crate::types::Message;
use crate::supabase::SupabaseClient;

pub struct AnypayEventsServer {
    event_dispatcher: Arc<EventDispatcher>,
    sessions: Arc<RwLock<HashMap<Uuid, Session>>>,
    addr: String,
    supabase: Arc<SupabaseClient>,
}

impl AnypayEventsServer {
    pub fn new(addr: &str, supabase_url: &str, supabase_anon_key: &str, supabase_service_role_key: &str) -> Self {
        AnypayEventsServer {
            event_dispatcher: Arc::new(EventDispatcher::new()),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            addr: addr.to_string(),
            supabase: Arc::new(SupabaseClient::new(supabase_url, supabase_anon_key, supabase_service_role_key)),
        }
    }

    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind(&self.addr).await?;
        tracing::info!("WebSocket server listening on: {}", self.addr);

        while let Ok((stream, addr)) = listener.accept().await {
            tracing::info!("New connection from: {}", addr);
            
            let event_dispatcher = self.event_dispatcher.clone();
            let sessions = self.sessions.clone();
            let supabase = self.supabase.clone();
            
            tokio::spawn(async move {
                if let Err(e) = Self::handle_connection(stream, event_dispatcher, sessions, supabase).await {
                    tracing::error!("Error handling connection: {}", e);
                }
            });
        }

        Ok(())
    }

    async fn handle_message(
        message: Message,
        session: &Session,
        event_dispatcher: &Arc<EventDispatcher>,
        supabase: &Arc<SupabaseClient>,
    ) -> serde_json::Value {
        match message {
            Message::Subscribe { sub_type, id } => {
                event_dispatcher.subscribe(session.clone(), &sub_type, &id).await;
                json!({
                    "status": "success",
                    "message": format!("Subscribed to {} {}", sub_type, id)
                })
            }
            Message::Unsubscribe { sub_type, id } => {
                event_dispatcher.unsubscribe(session.clone(), &sub_type, &id).await;
                json!({
                    "status": "success",
                    "message": format!("Unsubscribed from {} {}", sub_type, id)
                })
            }
            Message::FetchInvoice { id } => {
                match supabase.get_invoice(&id, true).await {
                    Ok(Some(invoice)) => json!({
                        "status": "success",
                        "data": invoice
                    }),
                    Ok(None) => json!({
                        "status": "error",
                        "message": "Invoice not found"
                    }),
                    Err(e) => json!({
                        "status": "error",
                        "message": format!("Error fetching invoice: {}", e)
                    }),
                }
            }
        }
    }

    async fn handle_connection(
        stream: TcpStream,
        event_dispatcher: Arc<EventDispatcher>,
        sessions: Arc<RwLock<HashMap<Uuid, Session>>>,
        supabase: Arc<SupabaseClient>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let ws_stream = accept_async(stream).await?;
        let (mut ws_sender, mut ws_receiver) = ws_stream.split();
        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        let session_id = Uuid::new_v4();
        let session = Session::new(session_id, sender);
        
        // Store the session
        sessions.write().await.insert(session_id, session.clone());

        // Spawn a task to forward messages from the channel to the websocket
        let _send_task = tokio::spawn(async move {
            while let Some(message) = receiver.next().await {
                if let Err(e) = ws_sender.send(message).await {
                    tracing::error!("Error sending message: {}", e);
                    break;
                }
            }
        });

        // Handle incoming messages
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(msg) => {
                    if let Ok(text) = msg.to_text() {
                        let response = match serde_json::from_str::<Message>(text) {
                            Ok(message) => {
                                Self::handle_message(
                                    message,
                                    &session,
                                    &event_dispatcher,
                                    &supabase,
                                ).await
                            }
                            Err(_) => json!({
                                "status": "error",
                                "message": "Invalid message format"
                            })
                        };

                        if let Err(e) = session.send(WsMessage::Text(response.to_string())) {
                            tracing::error!("Error sending response: {}", e);
                            break;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Error receiving message: {}", e);
                    break;
                }
            }
        }

        // Clean up session when connection closes
        sessions.write().await.remove(&session_id);
        tracing::info!("Connection closed for session: {}", session_id);
        Ok(())
    }
} 