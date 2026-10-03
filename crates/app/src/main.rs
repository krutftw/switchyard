#[tokio::main]
async fn main() -> std::process::ExitCode {
    switchyard_app::cli::run().await
}
