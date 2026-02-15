//! Read a file over NFS using libnfs bindings.
//!
//! Usage: cat [server] [export] [path]
//! Default: cat 127.0.0.1 / /hello.txt

use libnfs::{NfsClient, NfsVersion};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let server = args.get(1).map(|s| s.as_str()).unwrap_or("127.0.0.1");
    let export = args.get(2).map(|s| s.as_str()).unwrap_or("/");
    let path = args.get(3).map(|s| s.as_str()).unwrap_or("/hello.txt");

    let client = NfsClient::mount(server, export, NfsVersion::V4)
        .await
        .unwrap();
    let data = client.read_file(path).await.unwrap();
    print!("{}", String::from_utf8_lossy(&data));
}
