//! `ldm-testsrv ROOT [ADDR]` – prints `LISTEN <addr>` then serves forever.
#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let root = std::path::PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:0".into());
    let srv = ldm_testsrv::start(root, &addr).await?;
    println!("LISTEN {}", srv.addr);
    use std::io::Write;
    std::io::stdout().flush()?;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}
