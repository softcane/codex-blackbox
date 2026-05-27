use std::process::{Command, Stdio};

use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum UiLaunchPlan {
    Command { program: String, args: Vec<String> },
    Unsupported { platform: String },
}

pub(crate) fn platform_launch_plan() -> UiLaunchPlan {
    launch_plan_for_platform(std::env::consts::OS)
}

pub(crate) fn launch_plan_for_platform(platform: &str) -> UiLaunchPlan {
    match platform {
        "macos" => UiLaunchPlan::Command {
            program: "open".to_string(),
            args: vec!["-a".to_string(), "Codex".to_string()],
        },
        other => UiLaunchPlan::Unsupported {
            platform: other.to_string(),
        },
    }
}

pub(crate) fn render_launch_plan(plan: &UiLaunchPlan) -> String {
    match plan {
        UiLaunchPlan::Command { program, args } => {
            let mut command = vec![program.clone()];
            command.extend(args.iter().cloned());
            format!(
                "Codex Blackbox UI launch preview\nAction: start or focus local Codex Desktop\nCommand: {}\nNo processes will be killed or restarted by Codex Blackbox.\n",
                command.join(" ")
            )
        }
        UiLaunchPlan::Unsupported { platform } => format!(
            "Codex Blackbox UI launch preview\nAction: unsupported on {platform}\nStart or focus local Codex Desktop/IDE manually, then restart it after enable.\n"
        ),
    }
}

pub(crate) fn execute_launch_plan(plan: &UiLaunchPlan) -> Result<(), String> {
    let UiLaunchPlan::Command { program, args } = plan else {
        return Err("UI launch is unsupported on this platform".to_string());
    };
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| format!("failed to run {program}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with status {status}"))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn launch_plan_uses_safe_macos_open_without_killing_processes() {
        let plan = super::launch_plan_for_platform("macos");
        assert_eq!(
            plan,
            super::UiLaunchPlan::Command {
                program: "open".to_string(),
                args: vec!["-a".to_string(), "Codex".to_string()]
            }
        );
        let rendered = super::render_launch_plan(&plan);
        assert!(rendered.contains("open -a Codex"));
        assert!(rendered.contains("No processes will be killed or restarted"));
    }

    #[test]
    fn launch_plan_is_explicit_when_platform_is_unsupported() {
        let plan = super::launch_plan_for_platform("linux");
        assert_eq!(
            plan,
            super::UiLaunchPlan::Unsupported {
                platform: "linux".to_string()
            }
        );
        assert!(super::render_launch_plan(&plan).contains("unsupported on linux"));
    }
}
