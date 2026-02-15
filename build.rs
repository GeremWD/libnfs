fn main() {
    pkg_config::Config::new()
        .atleast_version("4.0.0")
        .probe("libnfs")
        .expect("libnfs not found via pkg-config. Install libnfs-dev (or libnfs-devel).");
}
