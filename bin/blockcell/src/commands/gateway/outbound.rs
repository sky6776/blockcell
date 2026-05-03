use super::*;
// ---------------------------------------------------------------------------
// Outbound → WebSocket broadcast bridge
// ---------------------------------------------------------------------------

/// Forwards outbound messages from the runtime to all connected WebSocket clients
pub(super) async fn outbound_to_ws_bridge(
    mut outbound_rx: mpsc::Receiver<blockcell_core::OutboundMessage>,
    ws_broadcast: broadcast::Sender<String>,
    channel_manager: Arc<ChannelManager>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    loop {
        tokio::select! {
            msg = outbound_rx.recv() => {
                let Some(msg) = msg else { break };
                // Forward to WebSocket clients as a message_done event.
                // Skip "ws" channel — the runtime already emits events directly via event_tx.
                // Also skip messages marked skip_ws_echo (e.g. from persist_and_deliver_final_response)
                // to prevent duplicate message_done events.
                // Still forward cron, subagent, and other internal channel results to WS clients.
                if msg.channel != "ws" && !msg.skip_ws_echo {
                    let event = WsEvent::MessageDone {
                        chat_id: msg.chat_id.clone(),
                        task_id: String::new(),
                        content: msg.content.clone(),
                        tool_calls: 0,
                        duration_ms: 0,
                        media: msg.media.clone(),
                    };
                    if let Ok(json) = serde_json::to_string(&event) {
                        let _ = ws_broadcast.send(json);
                    }
                }

                // Also dispatch to external channels (telegram, slack, etc.)
                if msg.channel != "ws" && msg.channel != "cli" && msg.channel != "http" {
                    if let Err(e) = channel_manager.dispatch_outbound_msg(&msg).await {
                        error!(error = %e, channel = %msg.channel, "Failed to dispatch outbound message");
                    }
                }
            }
            _ = shutdown_rx.recv() => {
                debug!("outbound_to_ws_bridge received shutdown signal");
                break;
            }
        }
    }
}
