#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]

use anyhow::Context;
use wm_ipc_client::{subscription_id, IpcClient};

pub async fn start(args: Vec<String>) -> anyhow::Result<()> {
  let mut client = IpcClient::connect().await?;

  let message = args[1..].join(" ");
  client
    .send(&message)
    .await
    .context("Failed to send command to IPC server.")?;

  // Replies are printed as the server sent them. Decoding them into
  // `wm-common` types first fails for some valid replies (e.g. binding
  // modes, whose commands don't deserialize from JSON).
  let client_response = client
    .client_response_text(&message)
    .await
    .context("Failed to receive response from IPC server.")?;

  match subscription_id(&client_response)? {
    // For event subscriptions, omit the initial response message and
    // continuously output subsequent event messages.
    Some(subscription_id) => loop {
      let event_subscription = client
        .event_subscription_text(&subscription_id)
        .await
        .context("Failed to receive response from IPC server.")?;

      println!("{event_subscription}");
    },
    // For all other messages, output and exit when the first response
    // message is received.
    None => {
      println!("{client_response}");
    }
  }

  Ok(())
}
