//! Standalone CLI runner for `TestLabServer` used by Playwright Brave E2E tests (`P1-T35`).

use std::io::{self, Write};
use std::process::ExitCode;

use vortex_test_lab::TestLabServer;

#[tokio::main]
async fn main() -> ExitCode {
    let Ok(server) = TestLabServer::spawn().await else {
        let _ = writeln!(io::stderr(), "failed to bind TestLabServer");
        return ExitCode::from(1);
    };

    {
        let mut out = io::stdout().lock();
        let _ = writeln!(out, "READY {}", server.base_url());
        let _ = out.flush();
    }

    let _ = tokio::signal::ctrl_c().await;
    server.shutdown();
    ExitCode::SUCCESS
}
