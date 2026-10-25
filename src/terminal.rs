use std::io::{self, IsTerminal};

pub fn color() -> bool {
    io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").is_ok_and(|t| t != "dumb")
}
pub fn paint(text: &str, code: &str) -> String {
    if color() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.into()
    }
}
pub fn heading(name: &str, detail: &str) {
    println!("{}  {}", paint(name, "1;36"), paint(detail, "2"));
}
pub fn field(label: &str, value: impl std::fmt::Display) {
    println!("  {}  {value}", paint(&format!("{label:<12}"), "2"));
}
pub fn state(value: &str) -> String {
    paint(
        value,
        match value {
            "running" | "enabled" | "connected" => "32",
            "failed" | "invalid" => "31",
            "backoff" | "stopping" => "33",
            _ => "2",
        },
    )
}
