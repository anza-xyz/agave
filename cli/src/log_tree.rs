//! Solana transaction logs arrive as a flat `Vec<String>`. We want a tree so
//! the natural CPI nesting surfaces instead of staying buried in line-by-line
//! text.
//!
//! The logs nest cleanly: `invoke` opens a frame, `success|failed:` closes
//! it. That's the Dyck language D-1 of balanced brackets, which a pushdown
//! automaton recognizes by construction. See
//! <https://en.wikipedia.org/wiki/Dyck_language>.
//!
//! So the parser is two layers: a per-line lexer (finite-state automaton,
//! FSA) feeds a single-state pushdown automaton (PDA). The FSA has no
//! memory across lines; the PDA's stack carries the nesting.
//!
//! # Layer 1: per-line classifier (FSA)
//!
//! `classify` plus the `strip_prefix` cascade maps one line to one token.
//! No memory across lines; pure regular language.
//!
//! ```text
//!              ┌─ "Program log: Instruction: <n>" -> Instruction(n)
//!              ├─ "Program log: <t>"              -> Msg(t)
//!              ├─ "Program data: <d>"             -> Data(d)
//!              ├─ "Program <p> invoke [k]"        -> Invoke(p)
//! [line] ──────┼─ "Program <p> success"           -> Status::Success
//!              ├─ "Program <p> failed: <m>"       -> Status::Failed(m)
//!              ├─ "Program <p> consumed N of M …" -> Consumed(N, M)
//!              └─ <anything else>                 -> Other(line)
//! ```
//!
//! # Layer 2: stream parser (PDA)
//!
//! One control state; the stack does all the work. Transitions read
//! `input, stack-top -> new-stack`:
//!
//! ```text
//!      ┌─────────────────┐
//!      │     running     │ ─┐   single state;
//!      │  stack: γ       │  │   self-loop on every input
//!      └─────────────────┘  │
//!              ^            │
//!              └────────────┘
//!
//!   Invoke(p),       γ          ->  Frame{p, Truncated} · γ     (PUSH)
//!   Status::S,       Frame · γ  ->  γ; attach to parent/roots   (POP)
//!   Status::F(m),    Frame · γ  ->  γ; attach to parent/roots   (POP)
//!   Consumed(cu),    Frame · γ  ->  Frame{…cu=cu} · γ           (MUTATE top)
//!   Msg/Data/Other,  Frame · γ  ->  Frame{…logs+=…} · γ         (MUTATE top)
//!   Instruction(n),  Frame · γ  ->  Frame{…name=n} · γ          (MUTATE top)
//!   EOF                         ->  drain stack as Truncated
//! ```
//!
//! Payload-only tokens (`Msg`/`Data`/`Other`) cannot alter stack shape, so a
//! stray runtime diagnostic mid-CPI can't corrupt the tree. Conversely,
//! anything that affects the stack must match the exact tokenized shape: an
//! invoke-shaped line with a malformed `[k]` bracket falls back to `Other`
//! rather than pushing a half-known frame.
//!
//! Truncation falls out of the EOF transition: pre-seeding `outcome:
//! Truncated` on PUSH means the drain just pops unmodified frames; no special
//! case for "stream ended mid-frame".

use {
    solana_pubkey::Pubkey,
    std::{fmt::Write, str::FromStr},
};

// `cargo tree` glyphs. Connectors go on a child's line; spines continue
// under a frame on lines that follow. 4 cols wide so nested frames align.
const CONN_BRANCH: &str = "├── ";
const CONN_LAST: &str = "└── ";
const SPINE_CONTINUE: &str = "│   ";
const SPINE_END: &str = "    ";

// Narrower spines (2 cols) for `>> log:` / `>> data:` rows so they slot
// under the header without aligning with sibling connectors.
const LOG_SPINE_CONTINUE: &str = "│ ";
const LOG_SPINE_END: &str = "  ";

/// Both values from a `Consumed(N, M)` token. Either both are emitted
/// (BPF programs) or neither (native programs outside the SBPF VM:
/// `ComputeBudget`, `BpfLoader`, precompiles). The native-program gap is
/// load-bearing for the totals below; see `transaction_total_cu`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComputeUnits {
    /// CU consumed by this frame as reported by the SBPF VM. Cumulative
    /// over CPI children of the same frame (so descending the tree would
    /// double-count), but does NOT include native-program CU sandwiched
    /// at the top level: `ComputeBudget` instructions, precompiles, and
    /// the `BpfLoader` never emit `Program X consumed N of M` lines.
    pub consumed: u64,
    /// CU remaining in the transaction's budget when this frame started.
    /// First top-level frame: full budget. Later frames: running remainder.
    pub available_at_start: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpiFrame {
    pub program_id: Pubkey,
    pub outcome: CpiOutcome,
    pub compute_units: Option<ComputeUnits>,
    pub instruction_name: Option<String>,
    /// `Msg` / `Data` / `Other` tokens accumulated while this frame was on
    /// the stack, in arrival order. Survives every outcome.
    pub logs: Vec<FrameLog>,
    pub children: Vec<CpiFrame>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameLog {
    /// `Msg(t)` and `Other(line)` tokens (see module-level FSA). `Other`
    /// lands here too: no destructured shape, and the renderer treats text
    /// payloads uniformly.
    Msg(String),
    /// `Data(d)` token: `sol_log_data` payload (Anchor's `emit!`). We keep
    /// the log term "data"; "event" semantics layer above via per-program
    /// IDLs.
    Data(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CpiOutcome {
    Success,
    Failed {
        message: Option<String>,
    },
    /// Frame still open at EOF. Distinct from `Failed`: we lost sight of
    /// it rather than observed it die.
    Truncated,
}

/// Drive the PDA from the module doc: one pass over `logs`, dispatch each
/// token against the current stack top.
pub fn cpi_tree(logs: &[String]) -> Vec<CpiFrame> {
    let mut roots: Vec<CpiFrame> = Vec::new();
    let mut stack: Vec<CpiFrame> = Vec::new();

    for log in logs {
        if let Some(name) = log.strip_prefix("Program log: Instruction: ") {
            // `Instruction(n)`: dispatcher convention (Anchor, SPL Token).
            // Drop any pre-handler `Msg` chatter; keep `Data` . First name
            // wins.
            if let Some(frame) = stack.last_mut() {
                frame
                    .logs
                    .retain(|entry| !matches!(entry, FrameLog::Msg(_)));
                if frame.instruction_name.is_none() {
                    frame.instruction_name = Some(name.to_string());
                }
            }
            continue;
        }
        if let Some(text) = log.strip_prefix("Program log: ") {
            if let Some(frame) = stack.last_mut() {
                frame.logs.push(FrameLog::Msg(text.to_string()));
            }
            continue;
        }
        if let Some(payload) = log.strip_prefix("Program data: ") {
            if let Some(frame) = stack.last_mut() {
                frame.logs.push(FrameLog::Data(payload.to_string()));
            }
            continue;
        }

        match classify(log) {
            LogLine::Invoke(program) => {
                let Ok(program_id) = Pubkey::from_str(&program) else {
                    continue;
                };
                // PUSH with `outcome: Truncated` pre-seeded; the EOF drain
                // below leaves it untouched if no status line arrives.
                stack.push(CpiFrame {
                    program_id,
                    outcome: CpiOutcome::Truncated,
                    compute_units: None,
                    instruction_name: None,
                    logs: Vec::new(),
                    children: Vec::new(),
                });
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
                frame.outcome = match status {
                    Status::Success => CpiOutcome::Success,
                    Status::Failed { message } => CpiOutcome::Failed { message },
                };
                push_into_parent_or_roots(frame, &mut stack, &mut roots);
            }
            LogLine::Other => {
                // `Other` bucketed as `Msg` per the PDA's payload-only row.
                // No frame open: drop it (defined behavior, not a panic).
                if let Some(frame) = stack.last_mut() {
                    frame.logs.push(FrameLog::Msg(log.clone()));
                }
            }
        }
    }

    // EOF transition: drain remaining frames; pre-seeded `Truncated` carries.
    while let Some(frame) = stack.pop() {
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

/// Thousand-separated integer (`53402` -> `"53,402"`). Used wherever CU
/// values land in user-facing output.
pub fn with_commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in s.bytes().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

/// Sum of top-level frames' `consumed`, or `None` if no frame reported CU
/// (e.g. an all-native-program transaction). `None` distinguishes "no
/// data" from the impossible "every program consumed zero".
///
/// Children are skipped because per-frame `consumed` is already cumulative
/// over CPIs; descending would double-count.
///
/// N.B. this is BPF-visible CU, not transaction-total CU. Native-program
/// instructions at the top level (`ComputeBudget`, precompiles, `BpfLoader`)
/// don't emit `Program X consumed N of M compute units` lines, so their cost
/// is invisible here. `TransactionStatusMeta::compute_units_consumed` sums
/// everything (native + BPF) and is generally higher than this value; the
/// two only agree for an all-BPF transaction with no `ComputeBudget` prefix.
/// We surface the BPF-visible number because that's all `logsSubscribe`
/// gives us; consumers that need parity with the explorer must hit
/// `getTransaction` for the authoritative meta.
pub fn transaction_total_cu(frames: &[CpiFrame]) -> Option<u64> {
    frames
        .iter()
        .filter_map(|f| f.compute_units.as_ref().map(|cu| cu.consumed))
        .fold(None, |acc, cu| Some(acc.unwrap_or(0) + cu))
}

/// Transaction CU budget: `available_at_start` of the first top-level
/// frame with CU data. `None` if no frame reported CU.
pub fn transaction_compute_budget(frames: &[CpiFrame]) -> Option<u64> {
    frames
        .iter()
        .find_map(|f| f.compute_units.as_ref().map(|cu| cu.available_at_start))
}

enum LogLine {
    Invoke(String),
    Consumed(ComputeUnits),
    Status(Status),
    Other,
}

enum Status {
    Success,
    Failed { message: Option<String> },
}

/// Layer-1 FSA: tokenize on spaces and match the slice shape at known
/// indices. Malformed structural shapes degrade to `Other` rather than
/// constructing partial tokens.
fn classify(log: &str) -> LogLine {
    let tokens: Vec<&str> = log.split(' ').collect();
    match tokens.as_slice() {
        ["Program", _name, "invoke", bracket] if parse_depth_bracket(bracket).is_some() => {
            LogLine::Invoke(tokens[1].to_string())
        }
        ["Program", _, "success"] => LogLine::Status(Status::Success),
        ["Program", _, "failed:", ..] => {
            // Pass the message body through unmodified, whatever whitespace
            // the runtime used.
            let raw = log.splitn(4, ' ').nth(3).unwrap_or("").trim();
            let message = (!raw.is_empty()).then(|| raw.to_string());
            LogLine::Status(Status::Failed { message })
        }
        [
            "Program",
            _,
            "consumed",
            consumed,
            "of",
            available,
            "compute",
            "units",
        ] => match (consumed.parse::<u64>(), available.parse::<u64>()) {
            (Ok(consumed), Ok(available_at_start)) => LogLine::Consumed(ComputeUnits {
                consumed,
                available_at_start,
            }),
            _ => LogLine::Other,
        },
        _ => LogLine::Other,
    }
}

/// `cargo tree`-style box-art under a synthetic header. The header acts as
/// a visible parent so a transaction's multiple top-level frames read as
/// siblings rather than flush-left strangers.
pub fn format_cpi_tree(header: &str, frames: &[CpiFrame]) -> String {
    let mut out = String::new();
    writeln!(out, "{header}").unwrap();
    let last_idx = frames.len().saturating_sub(1);
    for (i, frame) in frames.iter().enumerate() {
        write_frame(&mut out, frame, "", i == last_idx);
    }
    out
}

fn write_frame(out: &mut String, frame: &CpiFrame, prefix: &str, is_last: bool) {
    let connector = if is_last { CONN_LAST } else { CONN_BRANCH };
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
        write!(
            out,
            "({} / {} CU) ",
            with_commas(cu.consumed),
            with_commas(cu.available_at_start)
        )
        .unwrap();
    }
    writeln!(out, "{}", frame.program_id).unwrap();

    let child_prefix = if is_last {
        format!("{prefix}{SPINE_END}")
    } else {
        format!("{prefix}{SPINE_CONTINUE}")
    };
    // `LOG_SPINE_CONTINUE` (`│ `) when children follow; `LOG_SPINE_END`
    // (`  `) otherwise, to avoid a dangling `│` under a leaf frame.
    let log_spine = if frame.children.is_empty() {
        LOG_SPINE_END
    } else {
        LOG_SPINE_CONTINUE
    };
    for entry in &frame.logs {
        match entry {
            FrameLog::Msg(text) => {
                writeln!(out, "{child_prefix}{log_spine}>> log:  {text}").unwrap();
            }
            FrameLog::Data(payload) => {
                writeln!(out, "{child_prefix}{log_spine}>> data: {payload}").unwrap();
            }
        }
    }
    let last_idx = frame.children.len().saturating_sub(1);
    for (i, child) in frame.children.iter().enumerate() {
        write_frame(out, child, &child_prefix, i == last_idx);
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
    // Aliased to dodge the crate-root `pubkey!` macro_rules in `lib.rs`,
    // which has a totally unrelated clap-builder signature.
    use {super::*, solana_pubkey::pubkey as pk};

    // ---- Pubkey fixtures ----
    // Named program ids referenced across tests. The `pk!` macro is
    // `solana_pubkey::pubkey!`, which rejects invalid base58 or wrong byte
    // length at compile time, so a typo here fails the build rather than
    // silently dropping a frame from a test's expected tree.
    const SYSTEM_PROG: Pubkey = pk!("11111111111111111111111111111111");
    const TOKEN_PROG: Pubkey = pk!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
    const CONFIG_PROG: Pubkey = pk!("Config1111111111111111111111111111111111111");
    // Generic test programs (no specific real-world program). The identity
    // doesn't matter to the parser; we just need stable distinct pubkeys.
    const PROG_A: Pubkey = pk!("GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2");
    const PROG_B: Pubkey = pk!("6Ng7PojJBe6XjbsR65ftKKBpHUe2erD7E5dgGdMUjcgg");
    const PROG_C: Pubkey = pk!("22222222222222222222222222222222222222222222");

    // ---- Flat-log-line constructors ----
    // The total-CU value in `consumed` doesn't affect parsing; we hard-code a
    // generic 200000 so call sites stay short.
    fn invoke(pk: &Pubkey, depth: u8) -> String {
        format!("Program {pk} invoke [{depth}]")
    }
    fn instruction(name: &str) -> String {
        format!("Program log: Instruction: {name}")
    }
    fn program_log(text: &str) -> String {
        format!("Program log: {text}")
    }
    fn program_data(payload: &str) -> String {
        format!("Program data: {payload}")
    }
    fn consumed(pk: &Pubkey, cu: u64) -> String {
        format!("Program {pk} consumed {cu} of 200000 compute units")
    }
    fn success(pk: &Pubkey) -> String {
        format!("Program {pk} success")
    }
    fn failed(pk: &Pubkey, msg: &str) -> String {
        format!("Program {pk} failed: {msg}")
    }

    // ---- Render assertion ----
    /// Compare a rendered tree against a multi-line literal, after dedenting
    /// the literal by its common leading indent. Lets renderer tests write
    /// the expected output as it should appear, with normal source-code
    /// indentation, instead of as a long backslash-escaped string.
    fn assert_render_eq(actual: &str, expected: &str) {
        let normalized = dedent(expected);
        if actual != normalized {
            eprintln!("=== expected ===\n{normalized}");
            eprintln!("=== actual ===\n{actual}");
            panic!("render mismatch (diff above)");
        }
    }

    fn dedent(s: &str) -> String {
        // `trim_start_matches('\n')` drops the leading newline that comes
        // from writing the literal across multiple source lines. `trim_end`
        // drops any trailing whitespace, including the indent on the line
        // that holds the closing quote, so we don't carry a phantom blank
        // line into the output.
        let trimmed = s.trim_start_matches('\n').trim_end();
        let lines: Vec<&str> = trimmed.lines().collect();
        // Min leading-whitespace count across non-blank lines: that's the
        // common source indent. Blank lines are skipped or they'd pull the
        // dedent down to zero.
        let indent = lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.len() - l.trim_start().len())
            .min()
            .unwrap_or(0);
        let stripped: Vec<String> = lines
            .iter()
            .map(|l| {
                if l.len() >= indent {
                    l[indent..].to_string()
                } else {
                    l.to_string()
                }
            })
            .collect();
        let mut out = stripped.join("\n");
        out.push('\n'); // match the trailing-newline shape of writeln output
        out
    }

    #[test]
    fn empty_stream_yields_empty_tree() {
        assert!(cpi_tree(&[]).is_empty());
    }

    #[test]
    fn nested_cpi_attaches_child_under_parent() {
        let logs = vec![
            invoke(&PROG_A, 1),
            instruction("Mint"),
            invoke(&TOKEN_PROG, 2),
            instruction("MintTo"),
            consumed(&TOKEN_PROG, 1500),
            success(&TOKEN_PROG),
            consumed(&PROG_A, 5000),
            success(&PROG_A),
        ];
        let tree = cpi_tree(&logs);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].instruction_name.as_deref(), Some("Mint"));
        assert_eq!(
            tree[0].compute_units,
            Some(ComputeUnits {
                consumed: 5_000,
                available_at_start: 200_000,
            })
        );
        assert_eq!(tree[0].outcome, CpiOutcome::Success);
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(
            tree[0].children[0].instruction_name.as_deref(),
            Some("MintTo")
        );
    }

    #[test]
    fn failed_frame_carries_message_and_logs() {
        let logs = vec![
            invoke(&PROG_A, 1),
            instruction("Withdraw"),
            program_log("AnchorError caused by account: vault. Error Code: ConstraintHasOne."),
            failed(&PROG_A, "custom program error: 0x7d1"),
        ];
        let tree = cpi_tree(&logs);
        let CpiOutcome::Failed { message } = &tree[0].outcome else {
            panic!("expected Failed");
        };
        assert_eq!(message.as_deref(), Some("custom program error: 0x7d1"));
        assert_eq!(tree[0].logs.len(), 1);
        let FrameLog::Msg(text) = &tree[0].logs[0] else {
            panic!("expected Msg variant");
        };
        assert!(text.contains("ConstraintHasOne"));
    }

    #[test]
    fn unclosed_frames_drain_as_truncated() {
        let logs = vec![invoke(&PROG_A, 1), invoke(&TOKEN_PROG, 2)];
        let tree = cpi_tree(&logs);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].outcome, CpiOutcome::Truncated);
        assert_eq!(tree[0].children[0].outcome, CpiOutcome::Truncated);
    }

    #[test]
    fn unprefixed_runtime_line_captured_as_msg() {
        let logs = vec![
            invoke(&CONFIG_PROG, 1),
            // The bare runtime diagnostic. No `Program log:` prefix, no
            // structural shape; it's the cause of the upcoming failure.
            "account J2kSTGu6eod7MUAy2nNZhFW5ye5ZdhAri6bcJJHRhhXy signer_key().is_none()"
                .to_string(),
            failed(&CONFIG_PROG, "missing required signature for instruction"),
        ];
        let tree = cpi_tree(&logs);
        assert!(matches!(tree[0].outcome, CpiOutcome::Failed { .. }));
        assert_eq!(tree[0].logs.len(), 1);
        let FrameLog::Msg(text) = &tree[0].logs[0] else {
            panic!("expected Msg variant");
        };
        assert!(text.contains("signer_key().is_none()"));
    }

    #[test]
    fn format_nested_grandchild_extends_pipe_through_non_last_branch() {
        let logs = vec![
            invoke(&PROG_A, 1),
            instruction("Mint"),
            invoke(&TOKEN_PROG, 2),
            instruction("MintTo"),
            invoke(&SYSTEM_PROG, 3),
            consumed(&SYSTEM_PROG, 50),
            success(&SYSTEM_PROG),
            consumed(&TOKEN_PROG, 1500),
            success(&TOKEN_PROG),
            invoke(&PROG_C, 2),
            consumed(&PROG_C, 100),
            success(&PROG_C),
            consumed(&PROG_A, 5000),
            success(&PROG_A),
        ];
        let tree = cpi_tree(&logs);
        let out = format_cpi_tree("CPI Tree:", &tree);
        // TODO: declare tree DOM style
        assert_render_eq(
            &out,
            "
            CPI Tree:
            └── Mint (5,000 / 200,000 CU) GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2
                ├── MintTo (1,500 / 200,000 CU) TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA
                │   └── (50 / 200,000 CU) 11111111111111111111111111111111
                └── (100 / 200,000 CU) 22222222222222222222222222222222222222222222
            ",
        );
    }

    #[test]
    fn format_failed_frame_shows_message() {
        let logs = vec![
            invoke(&PROG_A, 1),
            instruction("Withdraw"),
            consumed(&PROG_A, 3100),
            failed(&PROG_A, "custom program error: 0x7d1"),
        ];
        let tree = cpi_tree(&logs);
        let out = format_cpi_tree("CPI Tree:", &tree);
        assert_render_eq(
            &out,
            "
            CPI Tree:
            └── Withdraw FAILED: custom program error: 0x7d1 (3,100 / 200,000 CU) \
             GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2
            ",
        );
    }

    #[test]
    fn format_empty_tree_yields_only_header() {
        assert_eq!(format_cpi_tree("CPI Tree:", &[]), "CPI Tree:\n");
    }

    #[test]
    fn format_multiple_roots_group_under_header() {
        // The case the synthetic header was introduced for: a transaction
        // with several top-level instructions reads as one tree, not as a
        // flush-left list of strangers.
        let logs = vec![
            invoke(&SYSTEM_PROG, 1),
            success(&SYSTEM_PROG),
            invoke(&TOKEN_PROG, 1),
            instruction("Foo"),
            consumed(&TOKEN_PROG, 200),
            success(&TOKEN_PROG),
            invoke(&PROG_A, 1),
            success(&PROG_A),
        ];
        let tree = cpi_tree(&logs);
        let out = format_cpi_tree("CPI Tree:", &tree);
        assert_render_eq(
            &out,
            "
            CPI Tree:
            ├── 11111111111111111111111111111111
            ├── Foo (200 / 200,000 CU) TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA
            └── GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2
            ",
        );
    }

    #[test]
    fn format_frame_with_data_no_children_uses_plain_pad() {
        // Frame has data lines but no children; the data lines terminate
        // under the frame so no spine is needed.
        let logs = vec![
            invoke(&PROG_B, 1),
            instruction("EmitTwo"),
            program_data("BqfPDIBaUMVcAAAA"),
            program_data("AnotherBase64String"),
            consumed(&PROG_B, 1500),
            success(&PROG_B),
        ];
        let tree = cpi_tree(&logs);
        let out = format_cpi_tree("CPI Tree:", &tree);
        assert_render_eq(
            &out,
            "
            CPI Tree:
            └── EmitTwo (1,500 / 200,000 CU) 6Ng7PojJBe6XjbsR65ftKKBpHUe2erD7E5dgGdMUjcgg
                  >> data: BqfPDIBaUMVcAAAA
                  >> data: AnotherBase64String
            ",
        );
    }

    #[test]
    fn format_frame_with_data_and_children_uses_spine() {
        // Frame has data AND children; the data lines use `│ ` so the spine
        // visually connects them to the children branching below.
        let logs = vec![
            invoke(&PROG_A, 1),
            instruction("Mint"),
            program_data("ParentDataPayload"),
            invoke(&TOKEN_PROG, 2),
            instruction("MintTo"),
            consumed(&TOKEN_PROG, 1500),
            success(&TOKEN_PROG),
            consumed(&PROG_A, 5000),
            success(&PROG_A),
        ];
        let tree = cpi_tree(&logs);
        let out = format_cpi_tree("CPI Tree:", &tree);
        assert_render_eq(
            &out,
            "
            CPI Tree:
            └── Mint (5,000 / 200,000 CU) GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2
                │ >> data: ParentDataPayload
                └── MintTo (1,500 / 200,000 CU) TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA
            ",
        );
    }

    #[test]
    fn format_frame_with_no_data_omits_data_lines() {
        let logs = vec![
            invoke(&SYSTEM_PROG, 1),
            consumed(&SYSTEM_PROG, 100),
            success(&SYSTEM_PROG),
        ];
        let tree = cpi_tree(&logs);
        let out = format_cpi_tree("CPI Tree:", &tree);
        assert_render_eq(
            &out,
            "
            CPI Tree:
            └── (100 / 200,000 CU) 11111111111111111111111111111111
            ",
        );
    }

    #[test]
    fn logs_and_data_preserve_interleaved_order() {
        // A handler that alternates `msg!` and `emit!` should produce a
        // `frame.logs` that interleaves Msg and Data variants in exactly the
        // order the runtime emitted them.
        let logs = vec![
            invoke(&PROG_B, 1),
            instruction("Mix"),
            program_log("step 1"),
            program_data("FirstData"),
            program_log("step 2"),
            program_data("SecondData"),
            program_log("step 3"),
            consumed(&PROG_B, 1500),
            success(&PROG_B),
        ];
        let tree = cpi_tree(&logs);
        assert_eq!(tree.len(), 1);
        use FrameLog::*;
        assert_eq!(
            tree[0].logs,
            vec![
                Msg("step 1".to_string()),
                Data("FirstData".to_string()),
                Msg("step 2".to_string()),
                Data("SecondData".to_string()),
                Msg("step 3".to_string()),
            ]
        );
    }

    #[test]
    fn format_interleaved_logs_and_data() {
        // Renderer should emit `>> log:` and `>> data:` entries in the same
        // arrival order they appear in `frame.logs`.
        let logs = vec![
            invoke(&PROG_B, 1),
            instruction("Mix"),
            program_log("step 1"),
            program_data("FirstData"),
            program_log("step 2"),
            consumed(&PROG_B, 1500),
            success(&PROG_B),
        ];
        let tree = cpi_tree(&logs);
        let out = format_cpi_tree("CPI Tree:", &tree);
        assert_render_eq(
            &out,
            "
            CPI Tree:
            └── Mix (1,500 / 200,000 CU) 6Ng7PojJBe6XjbsR65ftKKBpHUe2erD7E5dgGdMUjcgg
                  >> log:  step 1
                  >> data: FirstData
                  >> log:  step 2
            ",
        );
    }

    #[test]
    fn success_frame_captures_data_entries() {
        // `Program data: <base64>` lines come from `sol_log_data` (Anchor's
        // `emit!`). They survive a `Success` pop, unlike pre-this-refactor
        // diagnostics did not.
        let logs = vec![
            invoke(&PROG_B, 1),
            instruction("EmitTwo"),
            program_data("BqfPDIBaUMVcAAAA"),
            program_data("AnotherBase64String"),
            consumed(&PROG_B, 1500),
            success(&PROG_B),
        ];
        let tree = cpi_tree(&logs);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].outcome, CpiOutcome::Success);
        use FrameLog::*;
        assert_eq!(
            tree[0].logs,
            vec![
                Data("BqfPDIBaUMVcAAAA".to_string()),
                Data("AnotherBase64String".to_string()),
            ]
        );
    }

    #[test]
    fn parser_captures_consumed_and_available() {
        // The flat log line carries both X (consumed) and Y (available at
        // frame start). We must capture both so the renderer can surface
        // them; dropping Y would silently lose information the runtime
        // emitted.
        let logs = vec![
            invoke(&PROG_A, 1),
            // 1,500 CU consumed of 198,000 available at frame start.
            "Program GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2 consumed 1500 of 198000 compute \
             units"
                .to_string(),
            success(&PROG_A),
        ];
        let tree = cpi_tree(&logs);
        assert_eq!(
            tree[0].compute_units,
            Some(ComputeUnits {
                consumed: 1_500,
                available_at_start: 198_000,
            })
        );
    }

    #[test]
    fn with_commas_inserts_thousands_separators() {
        assert_eq!(with_commas(0), "0");
        assert_eq!(with_commas(42), "42");
        assert_eq!(with_commas(999), "999");
        assert_eq!(with_commas(1_000), "1,000");
        assert_eq!(with_commas(53_402), "53,402");
        assert_eq!(with_commas(1_234_567_890), "1,234,567,890");
    }

    #[test]
    fn transaction_total_cu_sums_top_level_frames() {
        // Three top-level frames with known CU; the helper should sum them.
        // Mirrors the Phoenix-style transaction whose Explorer total (53,402)
        // we verified matches the top-level-frame sum.
        let logs = vec![
            invoke(&PROG_A, 1),
            consumed(&PROG_A, 4_817),
            success(&PROG_A),
            invoke(&PROG_B, 1),
            consumed(&PROG_B, 9_497),
            success(&PROG_B),
            invoke(&TOKEN_PROG, 1),
            consumed(&TOKEN_PROG, 17_173),
            success(&TOKEN_PROG),
        ];
        let tree = cpi_tree(&logs);
        assert_eq!(transaction_total_cu(&tree), Some(31_487));
    }

    #[test]
    fn transaction_total_cu_returns_none_when_no_frame_has_cu() {
        // BPF Loader-only and System-Program-only transactions are real:
        // native programs don't emit `consumed` lines, so every top-level
        // frame ends up with `compute_units: None`. The helper must
        // distinguish that from a true zero so the renderer can label the
        // case explicitly instead of misreporting "0 CU".
        let logs = vec![invoke(&SYSTEM_PROG, 1), success(&SYSTEM_PROG)];
        let tree = cpi_tree(&logs);
        assert_eq!(tree[0].compute_units, None);
        assert_eq!(transaction_total_cu(&tree), None);
    }

    #[test]
    fn transaction_compute_budget_reads_first_available() {
        // The transaction budget is the `available_at_start` of the *first*
        // top-level frame that reported CU. Frames that came before it
        // without CU data (e.g. native ComputeBudget instructions) are
        // skipped over by `find_map`.
        let logs = vec![
            // No-CU native instruction first.
            invoke(&SYSTEM_PROG, 1),
            success(&SYSTEM_PROG),
            // Then a BPF program reporting `consumed 4817 of 1000000`.
            invoke(&PROG_A, 1),
            "Program GtdambwDgHWrDJdVPBkEHGhCwokqgAoch162teUjJse2 consumed 4817 of 1000000 \
             compute units"
                .to_string(),
            success(&PROG_A),
        ];
        let tree = cpi_tree(&logs);
        assert_eq!(transaction_compute_budget(&tree), Some(1_000_000));
    }

    #[test]
    fn transaction_total_cu_does_not_double_count_children() {
        // Per-frame `consumed` is cumulative in Solana: the parent's value
        // already includes any CPI children's consumption (verified against
        // Explorer for tx 2p5cKaWqMRiYZNfk7...). The helper must sum only
        // root frames; descending into children would double-count.
        let logs = vec![
            invoke(&PROG_A, 1),
            invoke(&TOKEN_PROG, 2),
            consumed(&TOKEN_PROG, 500),
            success(&TOKEN_PROG),
            consumed(&PROG_A, 1_500),
            success(&PROG_A),
        ];
        let tree = cpi_tree(&logs);
        // Sanity: parent's consumed is 1_500, child's is 500. The 500 is
        // *included* in the 1_500 (per-frame consumed is cumulative), so
        // the transaction total is 1_500, not 2_000.
        assert_eq!(tree[0].compute_units.map(|cu| cu.consumed), Some(1_500));
        assert_eq!(
            tree[0].children[0].compute_units.map(|cu| cu.consumed),
            Some(500)
        );
        assert_eq!(transaction_total_cu(&tree), Some(1_500));
    }
}
