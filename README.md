# nfs4

Pure-Rust async NFSv4 client. No C libraries, no `unsafe`. Talks NFSv4 (RFC 7530) directly over TCP using hand-written XDR encoding and ONC-RPC framing.

## Features

- **Open once, read many** — `NfsFile` handle for cheap random reads (1 round trip each)
- **Automatic lease renewal** — background task renews at half the server's advertised lease time
- **Automatic cleanup** — `NfsFile` sends CLOSE on drop
- **COMPOUND batching** — multiple ops per round trip where possible
- **No dependencies on C** — pure Rust, only depends on `tokio`, `thiserror`, `rand`

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
nfs4 = { path = "." }
tokio = { version = "1", features = ["rt", "macros"] }
```

### Open a file and read ranges

```rust
use nfs4::Nfs4Client;

#[tokio::main(flavor = "current_thread")]
async fn main() -> nfs4::Result<()> {
    let client = Nfs4Client::connect("127.0.0.1", "/").await?;

    let file = client.open("/data.bin").await?;
    let header = file.read(0, 64).await?;
    let chunk  = file.read(4096, 1024).await?;
    Ok(())
}
```

### Read a whole file

```rust
let client = Nfs4Client::connect("server", "/export").await?;
let file = client.open("/hello.txt").await?;

let mut offset = 0u64;
loop {
    let chunk = file.read(offset, 1024 * 1024).await?;
    if chunk.is_empty() {
        break;
    }
    print!("{}", String::from_utf8_lossy(&chunk));
    offset += chunk.len() as u64;
}
```

### List a directory

```rust
let entries = client.readdir("/").await?;
for entry in &entries {
    println!("{:>10}  {}", entry.attrs.size.unwrap_or(0), entry.name);
}
```

### Stat a file

```rust
let attrs = client.stat("/data.bin").await?;
println!("size = {}, mode = {:o}", attrs.size.unwrap_or(0), attrs.mode.unwrap_or(0));
```

## Examples

```bash
cargo run --example cat -- 127.0.0.1 / /hello.txt
cargo run --example ls  -- 127.0.0.1 / /
```


## NFS server setup (for testing)

```bash
# /etc/exports
/srv/nfs/test  127.0.0.1(rw,sync,fsid=0,insecure,no_subtree_check,no_root_squash)

sudo exportfs -ra
sudo systemctl restart nfs-server
echo "hello from NFS" | sudo tee /srv/nfs/test/hello.txt
```

