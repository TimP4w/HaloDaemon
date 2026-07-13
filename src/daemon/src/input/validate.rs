// SPDX-License-Identifier: GPL-3.0-or-later
//! Domain validation for input actions and macros — apply at every ingress.

use anyhow::{ensure, Result};
use halod_shared::types::{
    ButtonAction, ButtonMapping, MacroStep, MACRO_MAX_DELAY_MS, MACRO_MAX_STEPS, MACRO_MAX_TOTAL_MS,
};

/// Max modifiers in a key chord (every distinct ModKey variant is few).
const MAX_MODIFIERS: usize = 8;
/// Max magnitude of a single scroll action.
const MAX_SCROLL_CLICKS: i32 = 10_000;
/// Max number of arguments to a spawned command.
const MAX_CMD_ARGS: usize = 64;
/// Max length of an executable path / command / argument string.
const MAX_STR_LEN: usize = 4096;

/// Validate a macro step list: non-empty, within the step-count, per-step delay,
/// and aggregate-duration ceilings.
pub fn validate_macro(steps: &[MacroStep]) -> Result<()> {
    ensure!(!steps.is_empty(), "macro has no steps");
    ensure!(
        steps.len() <= MACRO_MAX_STEPS,
        "macro exceeds {MACRO_MAX_STEPS} steps"
    );
    ensure!(
        steps.iter().all(|s| s.delay_after_ms <= MACRO_MAX_DELAY_MS),
        "macro delay exceeds {MACRO_MAX_DELAY_MS} ms"
    );
    let total: u64 = steps.iter().map(|s| s.delay_after_ms as u64).sum();
    ensure!(
        total <= MACRO_MAX_TOTAL_MS,
        "macro total delay {total} ms exceeds {MACRO_MAX_TOTAL_MS} ms"
    );
    Ok(())
}

fn validate_str(s: &str, what: &str) -> Result<()> {
    ensure!(!s.is_empty(), "{what} must not be empty");
    ensure!(s.len() <= MAX_STR_LEN, "{what} exceeds {MAX_STR_LEN} bytes");
    ensure!(!s.contains('\0'), "{what} contains a NUL byte");
    Ok(())
}

/// Validate a single button action, recursing into nested macros.
pub fn validate_button_action(action: &ButtonAction) -> Result<()> {
    match action {
        ButtonAction::Scroll { clicks, .. } => {
            ensure!(
                clicks.unsigned_abs() as i64 <= MAX_SCROLL_CLICKS as i64,
                "scroll magnitude exceeds {MAX_SCROLL_CLICKS}"
            );
        }
        ButtonAction::KeyChord { modifiers, .. } => {
            ensure!(
                modifiers.len() <= MAX_MODIFIERS,
                "key chord has more than {MAX_MODIFIERS} modifiers"
            );
            for (i, m) in modifiers.iter().enumerate() {
                ensure!(
                    !modifiers[..i].contains(m),
                    "key chord has a duplicate modifier"
                );
            }
        }
        ButtonAction::MomentaryDpi { dpi } => {
            ensure!(*dpi > 0, "momentary DPI must be non-zero");
        }
        ButtonAction::Macro { steps } => validate_macro(steps)?,
        ButtonAction::OpenApp { path } => validate_str(path, "application path")?,
        ButtonAction::Command { cmd, args } => {
            validate_str(cmd, "command")?;
            ensure!(
                args.len() <= MAX_CMD_ARGS,
                "command has more than {MAX_CMD_ARGS} arguments"
            );
            for arg in args {
                ensure!(
                    arg.len() <= MAX_STR_LEN,
                    "command argument exceeds {MAX_STR_LEN} bytes"
                );
                ensure!(!arg.contains('\0'), "command argument contains a NUL byte");
            }
        }
        _ => {}
    }
    Ok(())
}

/// Validate both the base and shifted actions of a button mapping.
pub fn validate_button_mapping(mapping: &ButtonMapping) -> Result<()> {
    validate_button_action(&mapping.base)?;
    validate_button_action(&mapping.shifted)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::{MacroAtom, ModKey};

    fn delay_step(ms: u32) -> MacroStep {
        MacroStep {
            kind: MacroAtom::Delay,
            delay_after_ms: ms,
        }
    }

    #[test]
    fn macro_empty_rejected() {
        assert!(validate_macro(&[]).is_err());
    }

    #[test]
    fn macro_too_many_steps_rejected() {
        let steps = vec![delay_step(0); MACRO_MAX_STEPS + 1];
        assert!(validate_macro(&steps).is_err());
    }

    #[test]
    fn macro_per_step_delay_rejected() {
        assert!(validate_macro(&[delay_step(MACRO_MAX_DELAY_MS + 1)]).is_err());
    }

    #[test]
    fn macro_aggregate_delay_rejected() {
        // Each step under the per-step cap, but together over the total ceiling.
        let per = MACRO_MAX_DELAY_MS; // 60_000
        let n = (MACRO_MAX_TOTAL_MS / per as u64) as usize + 2;
        let steps = vec![delay_step(per); n];
        // step count stays under MACRO_MAX_STEPS for this arithmetic
        assert!(n <= MACRO_MAX_STEPS);
        assert!(validate_macro(&steps).is_err());
    }

    #[test]
    fn macro_within_all_limits_ok() {
        assert!(validate_macro(&[delay_step(100), delay_step(200)]).is_ok());
    }

    #[test]
    fn nested_macro_in_action_validated() {
        let action = ButtonAction::Macro {
            steps: vec![delay_step(MACRO_MAX_DELAY_MS + 1)],
        };
        assert!(validate_button_action(&action).is_err());
    }

    #[test]
    fn empty_open_app_path_rejected() {
        assert!(validate_button_action(&ButtonAction::OpenApp {
            path: String::new()
        })
        .is_err());
    }

    #[test]
    fn command_with_too_many_args_rejected() {
        let action = ButtonAction::Command {
            cmd: "x".into(),
            args: vec!["a".to_string(); MAX_CMD_ARGS + 1],
        };
        assert!(validate_button_action(&action).is_err());
    }

    #[test]
    fn duplicate_modifiers_rejected() {
        let action = ButtonAction::KeyChord {
            key: 0x04,
            modifiers: vec![ModKey::Ctrl, ModKey::Ctrl],
        };
        assert!(validate_button_action(&action).is_err());
    }

    #[test]
    fn native_action_ok() {
        assert!(validate_button_mapping(&ButtonMapping {
            cid: 1,
            base: ButtonAction::Native,
            shifted: ButtonAction::Native,
        })
        .is_ok());
    }
}
