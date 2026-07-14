use std::{env, error::Error};

use deep_swarm_mock_server::{MockState, serve};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let address = env::var("DEEP_SWARM_MOCK_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let api_key = env::var("DEEP_SWARM_MOCK_API_KEY").unwrap_or_else(|_| "test-key".into());
    let listener = TcpListener::bind(&address).await?;
    println!("DeepSwarm mock server listening on http://{address}");
    serve(listener, MockState::new(api_key)).await?;
    Ok(())
}
