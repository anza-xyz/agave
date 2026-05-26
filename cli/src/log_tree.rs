//! We have a transaction's logs as a flat `Vec<String>` and we want a nested
//! invocation tree. This module does that fold.
//!
//! The runtime emits four kinds of structural lines that we destructure to
//! build the tree:
//!
//! ```text
//! Program <id> invoke [n]
//! Program <id> consumed N of M compute units
//! Program <id> success
//! Program <id> failed: <message>
//! ```
//!
//! Anything else (a program's own `Program log: ...` messages, `Program
//! return: ...` data, runtime chatter like `account X signer_key().is_none()`,
//! etc.) we capture verbatim as a diagnostic attached to whichever frame is
//! currently open. We don't interpret it further; that's a layer above this
//! module.
//!
//! When a program does a CPI, the child's `invoke` / `success` lines
//! interleave with the parent's own logging, and the bracket in `invoke [n]`
//! reflects the call depth. (We don't actually use the bracket value to drive
//! the parse; it's only there to validate that the line is shaped like an
//! invoke. The depth falls out of how the stack pushes and pops naturally.)
//!
//! Everything that follows rests on one runtime invariant: a callee cannot
//! outlive its caller. SBF physically can't return control to a parent CPI
//! until every child it kicked off has finished. So the sequence of `invoke`
//! and `success` / `failed:` lines is **well-nested**: every "open" is
//! matched by a "close" that arrives before any earlier "open" closes. (Same
//! property that makes `({[]})` a legal arithmetic expression and `({)[]}` a
//! nonsense one.)
//!
//! Equivalently: closes happen in reverse order of opens. Which is exactly
//! the access pattern of a stack, so the algorithm essentially writes itself:
//!
//!   - `invoke` -> push a new frame
//!   - `success` or `failed:` -> pop (it'll always be the right one to close,
//!     by the invariant above)
//!   - everything in between -> annotate whatever is on top
//!
//! The only way the brackets fail to balance is truncation: the log stream
//! cuts off mid-transaction. (The process died, the RPC dropped frames,
//! the buffer filled up; take your pick.) For that case, every frame gets
//! seeded with `outcome: Truncated` on push, so anything left on the stack
//! at end-of-stream bubbles up with a useful, distinguishable outcome rather
//! than being silently dropped or quietly misattributed as `Failed`. (It
//! didn't technically fail; we just don't know what happened to it. "Absence
//! of evidence" and "evidence of absence" are not the same thing, and the
//! consumer of this tree may care about that distinction.)

use {
    solana_pubkey::Pubkey,
    std::{fmt::Write, str::FromStr},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpiFrame {
    pub program_id: Pubkey,
    pub outcome: CpiOutcome,
    pub compute_units: Option<u64>,
    pub instruction_name: Option<String>,
    pub children: Vec<CpiFrame>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CpiOutcome {
    Success,
    Failed {
        message: Option<String>,
        diagnostics: Vec<String>,
    },
    /// Frame whose closing line never arrived: the stream ended with this
    /// frame still open. Distinct from `Failed` because we don't actually
    /// know what happened to it; we just lost sight of it.
    Truncated,
}

/// Walk the log stream once with a stack of in-progress frames. The mapping
/// is the obvious one: `invoke` pushes a frame, `success` / `failed:` pops
/// it, `consumed` and `Program log: ...` lines annotate whatever is on top.
///
/// `diags` runs as a second stack parallel to `stack` (one buffer per frame).
/// Keeping the two separate means a successful pop can throw the diag buffer
/// away cheaply rather than having to clone it into the frame just to then
/// discard it; the buffer only ever gets embedded into a frame when the
/// frame ends in `Failed`.
pub fn cpi_tree(logs: &[String]) -> Vec<CpiFrame> {
    let mut roots: Vec<CpiFrame> = Vec::new();
    let mut stack: Vec<CpiFrame> = Vec::new();
    let mut diags: Vec<Vec<String>> = Vec::new();

    for log in logs {
        if let Some(name) = log.strip_prefix("Program log: Instruction: ") {
            // The `Instruction:` line is what programs that follow the
            // convention (Anchor's dispatch macro, SPL Token) emit right
            // before the handler body runs. Anything we've buffered up to
            // this point is by definition pre-handler chatter, so clear it:
            // we want this frame's diagnostics to be what the handler said,
            // not what came before. In practice the buffer is usually empty
            // here (Anchor and SPL Token don't emit logs between invoke and
            // their Instruction: line), so this is mostly defensive.
            if let Some(top) = diags.last_mut() {
                top.clear();
            }
            // First `Instruction:` line wins. Defensive: in practice the
            // programs I've looked at (Anchor's dispatch, SPL Token) emit
            // exactly one such line per handler, so this check is just
            // belt-and-braces. But if a program ever does emit a second one
            // mid-handler, the first one is almost certainly the canonical
            // name and last-wins would be the wrong default.
            if let Some(frame) = stack.last_mut() {
                if frame.instruction_name.is_none() {
                    frame.instruction_name = Some(name.to_string());
                }
            }
            continue;
        }
        if let Some(diag) = log.strip_prefix("Program log: ") {
            if let Some(top) = diags.last_mut() {
                top.push(diag.to_string());
            }
            continue;
        }

        match classify(log) {
            LogLine::Invoke(program) => {
                let Ok(program_id) = Pubkey::from_str(&program) else {
                    continue;
                };
                // Pre-seed `outcome: Truncated`. If the frame never sees a
                // status line (because the stream got cut off), this is the
                // outcome it'll bubble up with, no special case in the
                // happy path needed.
                stack.push(CpiFrame {
                    program_id,
                    outcome: CpiOutcome::Truncated,
                    compute_units: None,
                    instruction_name: None,
                    children: Vec::new(),
                });
                diags.push(Vec::new());
            }
            LogLine::Consumed(cu) => {
                if let Some(frame) = stack.last_mut() {
                    frame.compute_units = Some(cu);
                }
            }
            LogLine::Status(status) => {
                let Some(mut frame) = stack.pop() else {
                    continue;
                };
                let frame_diags = diags.pop().unwrap_or_default();
                frame.outcome = match status {
                    Status::Success => CpiOutcome::Success,
                    Status::Failed { message } => CpiOutcome::Failed {
                        message,
                        diagnostics: frame_diags,
                    },
                };
                push_into_parent_or_roots(frame, &mut stack, &mut roots);
            }
            LogLine::Other => {
                // The catch-all: lines that aren't `invoke` / `consumed` /
                // status and don't start with `Program log:`. In practice
                // this is where `process_instruction:` and `solana_runtime:`
                // chatter ends up, plus the runtime's bare diagnostic lines
                // (the classic example being `account X signer_key().is_none()`
                // emitted before a Config-program signer-failure). These are
                // often the most useful thing in a failure trace, so we keep
                // them as raw diagnostics on the current frame.
                //
                // No current frame ⇒ nowhere to attach ⇒ drop it. (Doesn't
                // happen today, but it's a defined behavior rather than a
                // panic so future runtime changes can't blow us up here.)
                if let Some(top) = diags.last_mut() {
                    top.push(log.clone());
                }
            }
        }
    }

    // End-of-stream drain. Anything still on the stack at this point is a
    // frame we never saw close, so its pre-seeded `Truncated` outcome is what
    // gets used. The innermost frame pops first, which means it attaches to
    // its truncated parent as a child before the parent itself bubbles up;
    // the resulting hierarchy mirrors what we *would* have seen if the
    // stream had completed.
    while let Some(frame) = stack.pop() {
        let _ = diags.pop();
        push_into_parent_or_roots(frame, &mut stack, &mut roots);
    }

    roots
}

fn push_into_parent_or_roots(
    frame: CpiFrame,
    stack: &mut Vec<CpiFrame>,
    roots: &mut Vec<CpiFrame>,
) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(frame);
    } else {
        roots.push(frame);
    }
}

enum LogLine {
    Invoke(String),
    Consumed(u64),
    Status(Status),
    Other,
}

enum Status {
    Success,
    Failed { message: Option<String> },
}

/// Decide what kind of line we're looking at. Tokenize on single spaces and
/// pattern-match the slice shape, anchoring on the keyword positions
/// (`invoke`/`success`/`failed:`/`consumed` etc. at known indices) rather
/// than scanning for keywords anywhere in the line. The program id at
/// token[1] is opaque to us; we never compare against it.
fn classify(log: &str) -> LogLine {
    let tokens: Vec<&str> = log.split(' ').collect();
    match tokens.as_slice() {
        ["Program", _name, "invoke", bracket] if parse_depth_bracket(bracket).is_some() => {
            LogLine::Invoke(tokens[1].to_string())
        }
        ["Program", _, "success"] => LogLine::Status(Status::Success),
        ["Program", _, "failed:", ..] => {
            // `splitn(4, ' ')` peels off the three structural tokens
            // ("Program", program id, "failed:") and hands back the rest as
            // a slice of the original log. We extract just enough to
            // disambiguate the line shape; the message body comes through
            // unmodified, whatever whitespace the runtime used.
            let raw = log.splitn(4, ' ').nth(3).unwrap_or("").trim();
            let message = (!raw.is_empty()).then(|| raw.to_string());
            LogLine::Status(Status::Failed { message })
        }
        ["Program", _, "consumed", cu, "of", _, "compute", "units"] => {
            cu.parse::<u64>().map_or(LogLine::Other, LogLine::Consumed)
        }
        _ => LogLine::Other,
    }
}

/// Render `frames` as `cargo tree`-style box-art. Three rules cover it:
///
///   - root frames sit flush at column zero. (Yes, one transaction can have
///     several roots; each top-level instruction is its own root.)
///   - non-last children use `├── `, last child uses `└── `
///   - continuation under a non-last branch uses `│   ` to keep the vertical
///     spine visible. Continuation under a last branch is plain spaces; no
///     spine, because nothing further down branches off it.
pub fn format_cpi_tree(frames: &[CpiFrame]) -> String {
    let mut out = String::new();
    for frame in frames {
        write_frame(&mut out, frame, "", true, true);
    }
    out
}

fn write_frame(out: &mut String, frame: &CpiFrame, prefix: &str, is_last: bool, is_root: bool) {
    let connector = if is_root {
        ""
    } else if is_last {
        "└── "
    } else {
        "├── "
    };
    write!(out, "{prefix}{connector}").unwrap();
    if let Some(name) = &frame.instruction_name {
        write!(out, "{name} ").unwrap();
    }
    match &frame.outcome {
        CpiOutcome::Success => {}
        CpiOutcome::Failed { message, .. } => {
            write!(out, "FAILED: {} ", message.as_deref().unwrap_or("")).unwrap();
        }
        CpiOutcome::Truncated => write!(out, "TRUNCATED ").unwrap(),
    }
    if let Some(cu) = frame.compute_units {
        write!(out, "({cu} CU) ").unwrap();
    }
    writeln!(out, "{}", frame.program_id).unwrap();

    let child_prefix = if is_root {
        String::new()
    } else if is_last {
        format!("{prefix}    ")
    } else {
        format!("{prefix}│   ")
    };
    let last_idx = frame.children.len().saturating_sub(1);
    for (i, child) in frame.children.iter().enumerate() {
        write_frame(out, child, &child_prefix, i == last_idx, false);
    }
}

fn parse_depth_bracket(token: &str) -> Option<usize> {
    token
        .strip_prefix('[')?
        .strip_suffix(']')?
        .parse::<usize>()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn empty_stream_yields_empty_tree() {
        assert!(cpi_tree(&[]).is_empty());
    }

    #[test]
    fn nested_cpi_attaches_child_under_parent() {
        let logs = lines(&[
            "Program GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2 invoke [1]",
            "Program log: Instruction: Mint",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA invoke [2]",
            "Program log: Instruction: MintTo",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA consumed 1500 of 198000 compute \
             units",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA success",
            "Program GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2 consumed 5000 of 200000 compute \
             units",
            "Program GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2 success",
        ]);
        let tree = cpi_tree(&logs);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].instruction_name.as_deref(), Some("Mint"));
        assert_eq!(tree[0].compute_units, Some(5000));
        assert_eq!(tree[0].outcome, CpiOutcome::Success);
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(
            tree[0].children[0].instruction_name.as_deref(),
            Some("MintTo")
        );
    }

    #[test]
    fn failed_frame_carries_message_and_diagnostics() {
        let logs = lines(&[
            "Program GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2 invoke [1]",
            "Program log: Instruction: Withdraw",
            "Program log: AnchorError caused by account: vault. Error Code: ConstraintHasOne.",
            "Program GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2 failed: custom program error: \
             0x7d1",
        ]);
        let tree = cpi_tree(&logs);
        let CpiOutcome::Failed {
            message,
            diagnostics,
        } = &tree[0].outcome
        else {
            panic!("expected Failed");
        };
        assert_eq!(message.as_deref(), Some("custom program error: 0x7d1"));
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].contains("ConstraintHasOne"));
    }

    #[test]
    fn unclosed_frames_drain_as_truncated() {
        let logs = lines(&[
            "Program GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2 invoke [1]",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA invoke [2]",
        ]);
        let tree = cpi_tree(&logs);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].outcome, CpiOutcome::Truncated);
        assert_eq!(tree[0].children[0].outcome, CpiOutcome::Truncated);
    }

    #[test]
    fn unprefixed_runtime_diagnostic_captured() {
        let logs = lines(&[
            "Program Config1111111111111111111111111111111111111 invoke [1]",
            "account J2kSTGu6eod7MUAy2nNZhFW5ye5ZdhAri6bcJJHRhhXy signer_key().is_none()",
            "Program Config1111111111111111111111111111111111111 failed: missing required \
             signature for instruction",
        ]);
        let tree = cpi_tree(&logs);
        let CpiOutcome::Failed { diagnostics, .. } = &tree[0].outcome else {
            panic!("expected Failed");
        };
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].contains("signer_key().is_none()"));
    }

    #[test]
    fn format_nested_grandchild_extends_pipe_through_non_last_branch() {
        let logs = lines(&[
            "Program GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2 invoke [1]",
            "Program log: Instruction: Mint",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA invoke [2]",
            "Program log: Instruction: MintTo",
            "Program 11111111111111111111111111111111 invoke [3]",
            "Program 11111111111111111111111111111111 consumed 50 of 197000 compute units",
            "Program 11111111111111111111111111111111 success",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA consumed 1500 of 198000 compute \
             units",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA success",
            "Program 22222222222222222222222222222222222222222222 invoke [2]",
            "Program 22222222222222222222222222222222222222222222 consumed 100 of 198000 compute \
             units",
            "Program 22222222222222222222222222222222222222222222 success",
            "Program GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2 consumed 5000 of 200000 compute \
             units",
            "Program GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2 success",
        ]);
        let tree = cpi_tree(&logs);
        let out = format_cpi_tree(&tree);
        assert_eq!(
            out,
            "Mint (5000 CU) GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2\n├── MintTo (1500 CU) \
             TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA\n│   └── (50 CU) \
             11111111111111111111111111111111\n└── (100 CU) \
             22222222222222222222222222222222222222222222\n"
        );
    }

    #[test]
    fn format_failed_frame_shows_message() {
        let logs = lines(&[
            "Program GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2 invoke [1]",
            "Program log: Instruction: Withdraw",
            "Program GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2 consumed 3100 of 200000 compute \
             units",
            "Program GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2 failed: custom program error: \
             0x7d1",
        ]);
        let tree = cpi_tree(&logs);
        let out = format_cpi_tree(&tree);
        assert_eq!(
            out,
            "Withdraw FAILED: custom program error: 0x7d1 (3100 CU) \
             GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2\n"
        );
    }

    #[test]
    fn format_empty_tree_yields_empty_string() {
        assert_eq!(format_cpi_tree(&[]), "");
    }
}
