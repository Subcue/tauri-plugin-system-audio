const COMMANDS: &[&str] = &["start", "stop", "permission_status"];

fn main() {
    tauri_plugin::Builder::new(COMMANDS).build();
}
