//! List a directory over NFS using libnfs bindings.
//!
//! Usage: ls [server] [export] [path]
//! Default: ls 127.0.0.1 / /

use libnfs::{NfsClient, NfsVersion};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let server = args.get(1).map(|s| s.as_str()).unwrap_or("127.0.0.1");
    let export = args.get(2).map(|s| s.as_str()).unwrap_or("/");
    let path = args.get(3).map(|s| s.as_str()).unwrap_or("/");

    let client = NfsClient::mount(server, export, NfsVersion::V4)
        .await
        .unwrap();
    let entries = client.readdir(path).await.unwrap();

    for name in &entries {
        let st = client
            .stat(&format!("{}/{}", path.trim_end_matches('/'), name))
            .await;
        match st {
            Ok(stat) => {
                let ftype = if stat.mode & 0o40000 != 0 {
                    "d"
                } else if stat.mode & 0o120000 == 0o120000 {
                    "l"
                } else {
                    "-"
                };
                println!("{} {:>10}  {}", ftype, stat.size, name);
            }
            Err(_) => {
                println!("? {:>10}  {}", "?", name);
            }
        }
    }

    println!("\n{} entries", entries.len());
}
