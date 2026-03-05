use std::process::Command;
use std::sync::OnceLock;

fn command_exists(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

pub fn runtime_binary() -> &'static str {
    static BIN: OnceLock<&'static str> = OnceLock::new();
    BIN.get_or_init(|| {
        if command_exists("docker") {
            "docker"
        } else if command_exists("podman") {
            "podman"
        } else {
            // Keep docker as default so existing failure messages remain actionable.
            "docker"
        }
    })
}
