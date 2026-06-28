#[tokio::main]
async fn main() {
    std::process::exit(qgh::run().await);
}
