//! Read a file over NFS using libnfs bindings.
//!
//! Usage: cat [server] [export] [path]
//! Default: cat 127.0.0.1 / /hello.txt

use libnfs::{Client, Version};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let server = args.get(1).map(|s| s.as_str()).unwrap_or("127.0.0.1");
    let export = args.get(2).map(|s| s.as_str()).unwrap_or("/");
    let path = args.get(3).map(|s| s.as_str()).unwrap_or("/hello.txt");

    let client = Client::mount(server, export, Version::V4).await.unwrap();
    let st = client.stat(path).await.unwrap();
    let file = client.open(path).await.unwrap();
    let mut buf = vec![0u8; st.size as usize];
    let mut offset: usize = 0;
    while offset < buf.len() {
        let n = file
            .read_at(&mut buf[offset..], offset as u64)
            .await
            .unwrap();
        if n == 0 {
            break;
        }
        offset += n;
    }
    file.close().await.unwrap();
    print!("{}", String::from_utf8_lossy(&buf[..offset]));
}
