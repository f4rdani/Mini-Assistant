#[tokio::main]
async fn main() -> anyhow::Result<()> {
    api_ai::run_standalone().await
}
