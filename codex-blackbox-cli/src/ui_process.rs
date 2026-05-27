use std::process::{Command, Stdio};

use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct CodexUiProcess {
    pub(crate) pid: u32,
    pub(crate) command: String,
}

pub(crate) fn detect_codex_ui_processes() -> Vec<CodexUiProcess> {
    let Ok(output) = Command::new("ps")
        .args(["-axo", "pid=,command="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_ps_output(&text)
}

pub(crate) fn parse_ps_output(text: &str) -> Vec<CodexUiProcess> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let (pid, command) = trimmed.split_once(char::is_whitespace)?;
            let pid = pid.parse::<u32>().ok()?;
            let command = command.trim().to_string();
            is_codex_ui_process(&command).then_some(CodexUiProcess { pid, command })
        })
        .collect()
}

fn is_codex_ui_process(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.contains("codex")
        && (lower.contains("app-server")
            || lower.contains("codex desktop")
            || lower.contains("codex.app")
            || lower.contains("openai codex"))
        && !lower.contains("codex-blackbox")
}

#[cfg(test)]
mod tests {
    #[test]
    fn process_detection_finds_local_desktop_and_app_server_processes_only() {
        let processes = super::parse_ps_output(
            r#"
  101 /Applications/Codex.app/Contents/MacOS/Codex
  102 /usr/local/bin/codex app-server --port 1234
  103 codex-blackbox ui status
  104 /usr/local/bin/codex exec hello
  105 /Applications/Other.app/Contents/MacOS/Other
"#,
        );

        assert_eq!(processes.len(), 2);
        assert_eq!(processes[0].pid, 101);
        assert!(processes[0].command.contains("Codex.app"));
        assert_eq!(processes[1].pid, 102);
        assert!(processes[1].command.contains("app-server"));
    }
}
