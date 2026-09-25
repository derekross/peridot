//! Desktop notifications. Clicking one opens Peridot's panel.

/// "3 settings changed on Desk".
pub fn changes_waiting(count: usize, from: &str) {
    let from = clean(from);
    let title = if count == 1 {
        format!("A setting changed on {from}")
    } else {
        format!("{count} settings changed on {from}")
    };
    send(&title, "Review and apply them in Peridot.");
}

fn send(title: &str, body: &str) {
    let mut cmd;
    if which("omarchy-notification-send") {
        cmd = std::process::Command::new("omarchy-notification-send");
        cmd.args([
            "--app-name",
            "Peridot",
            "-g",
            "󰓦",
            "-t",
            "10000",
            title,
            body,
        ])
        .args(["--exec", "omarchy-shell", "derekross.peridot", "open"]);
    } else {
        cmd = std::process::Command::new("notify-send");
        cmd.args(["--app-name=Peridot", "--", title, body]);
    }
    if let Ok(mut child) = cmd.spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Device names come from other computers: one line, no markup, can't
/// start with a dash (it would be read as an option).
fn clean(s: &str) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let out: String = flat
        .chars()
        .take(40)
        .collect::<String>()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    out.trim_start_matches('-').to_string()
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn cleans_names() {
        assert_eq!(
            super::clean("--exec <b>Desk</b>\n"),
            "exec &lt;b&gt;Desk&lt;/b&gt;"
        );
    }
}
