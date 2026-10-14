//! `cargo run -p gate-db --example open -- <路径>`：用 Rust 的 bootstrap 打开（或新建）一个库。往返测试用。
fn main() {
    let path = std::env::args().nth(1).expect("用法：open <db 路径>");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let db = gate_db::Db::open(&path, now, &[]).expect("打开失败");
    drop(db);
    std::thread::sleep(std::time::Duration::from_millis(100));
    println!("ok {path}");
}
