#![allow(clippy::missing_errors_doc)]

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
  connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream,
};
use uuid::Uuid;
use wm_common::{
  ClientResponseMessage, EventSubscriptionMessage, ServerMessage,
  DEFAULT_IPC_PORT,
};

pub struct IpcClient {
  stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl IpcClient {
  pub async fn connect() -> anyhow::Result<Self> {
    let server_addr = format!("ws://127.0.0.1:{DEFAULT_IPC_PORT}");

    let (stream, _) = connect_async(server_addr)
      .await
      .context("Failed to connect to IPC server.")?;

    Ok(Self { stream })
  }

  /// Sends a message to the IPC server.
  pub async fn send(&mut self, message: &str) -> anyhow::Result<()> {
    self
      .stream
      .send(Message::Text(message.into()))
      .await
      .context("Failed to send command.")?;

    Ok(())
  }

  /// Waits and returns the next reply from the IPC server.
  pub async fn next_message(&mut self) -> anyhow::Result<ServerMessage> {
    let response = self
      .stream
      .next()
      .await
      .context("Failed to receive response.")?
      .context("Invalid response message.")?;

    let json_response =
      serde_json::from_str::<ServerMessage>(response.to_text()?)?;

    Ok(json_response)
  }

  pub async fn client_response(
    &mut self,
    client_message: &str,
  ) -> Option<ClientResponseMessage> {
    while let Ok(response) = self.next_message().await {
      if let ServerMessage::ClientResponse(client_response) = response {
        if client_response.client_message == client_message {
          return Some(client_response);
        }
      }
    }

    None
  }

  pub async fn event_subscription(
    &mut self,
    subscription_id: &Uuid,
  ) -> Option<EventSubscriptionMessage> {
    while let Ok(response) = self.next_message().await {
      if let ServerMessage::EventSubscription(event_sub) = response {
        if &event_sub.subscription_id == subscription_id {
          return Some(event_sub);
        }
      }
    }

    None
  }

  /// Waits for the reply to `client_message` and returns it exactly as
  /// the server sent it.
  ///
  /// Unlike [`IpcClient::client_response`], this doesn't decode the reply
  /// into [`ServerMessage`], which fails for replies the server can write
  /// but `wm-common` can't read back (e.g. `query binding-modes` while a
  /// binding mode is on: its commands are serialized as objects).
  pub async fn client_response_text(
    &mut self,
    client_message: &str,
  ) -> anyhow::Result<String> {
    loop {
      let text = self.next_text().await?;
      if is_reply(&text, "client_response", "clientMessage", client_message)?
      {
        return Ok(text);
      }
    }
  }

  /// Waits for the next event of the given subscription and returns it
  /// exactly as the server sent it.
  pub async fn event_subscription_text(
    &mut self,
    subscription_id: &Uuid,
  ) -> anyhow::Result<String> {
    let subscription_id = subscription_id.to_string();
    loop {
      let text = self.next_text().await?;
      if is_reply(
        &text,
        "event_subscription",
        "subscriptionId",
        &subscription_id,
      )? {
        return Ok(text);
      }
    }
  }

  async fn next_text(&mut self) -> anyhow::Result<String> {
    let response = self
      .stream
      .next()
      .await
      .context("Connection closed by IPC server.")?
      .context("Invalid response message.")?;

    Ok(response.to_text()?.to_string())
  }
}

/// Whether `text` is a server message of `message_type` whose `key` field
/// equals `value`.
fn is_reply(
  text: &str,
  message_type: &str,
  key: &str,
  value: &str,
) -> anyhow::Result<bool> {
  let json: serde_json::Value =
    serde_json::from_str(text).context("Invalid JSON from IPC server.")?;

  Ok(
    json["messageType"].as_str() == Some(message_type)
      && json[key].as_str() == Some(value),
  )
}

/// The subscription ID of an event subscription reply, if `text` is one.
pub fn subscription_id(text: &str) -> anyhow::Result<Option<Uuid>> {
  let json: serde_json::Value =
    serde_json::from_str(text).context("Invalid JSON from IPC server.")?;

  json["data"]["subscriptionId"]
    .as_str()
    .map(|id| Uuid::parse_str(id).context("Invalid subscription ID."))
    .transpose()
}

#[cfg(test)]
mod tests {
  use wm_common::{
    BindingModeConfig, BindingModesData, ClientResponseData,
    ClientResponseMessage, EventSubscribeData, ServerMessage,
  };

  use super::*;

  /// What the server sends for `query binding-modes` while resize mode is
  /// on (the same `serde_json::to_string(&ServerMessage)` the server uses).
  fn binding_modes_reply() -> String {
    let mode: BindingModeConfig = serde_json::from_str(
      r#"{"name":"resize","keybindings":[
        {"bindings":["h","alt+left"],"commands":["resize --width -2%"]},
        {"bindings":["escape"],"commands":["wm-disable-binding-mode --name resize"]}
      ]}"#,
    )
    .unwrap();

    reply(ClientResponseData::BindingModes(BindingModesData {
      binding_modes: vec![mode],
    }))
  }

  fn reply(data: ClientResponseData) -> String {
    serde_json::to_string(&ServerMessage::ClientResponse(
      ClientResponseMessage {
        client_message: "query binding-modes".to_string(),
        data: Some(data),
        error: None,
        success: true,
      },
    ))
    .unwrap()
  }

  #[test]
  fn typed_decode_rejects_an_active_binding_mode() {
    // Why `client_response_text` exists: the typed path can't read this.
    assert!(serde_json::from_str::<ServerMessage>(&binding_modes_reply())
      .is_err());
  }

  #[test]
  fn reply_with_an_active_binding_mode_is_recognised() {
    let text = binding_modes_reply();
    assert!(is_reply(
      &text,
      "client_response",
      "clientMessage",
      "query binding-modes"
    )
    .unwrap());
    assert!(!is_reply(
      &text,
      "client_response",
      "clientMessage",
      "query windows"
    )
    .unwrap());
    assert!(!is_reply(
      &text,
      "event_subscription",
      "clientMessage",
      "query binding-modes"
    )
    .unwrap());
    assert_eq!(subscription_id(&text).unwrap(), None);
  }

  #[test]
  fn subscription_id_is_read_from_a_subscribe_reply() {
    let id = Uuid::new_v4();
    let text = reply(ClientResponseData::EventSubscribe(EventSubscribeData {
      subscription_id: id,
    }));
    assert_eq!(subscription_id(&text).unwrap(), Some(id));
  }
}
