//! One bound on how much captured subprocess output may reach a model.
//!
//! Extracted from `verify.rs`, which had the only implementation, because
//! `tools.rs`'s shell tool had **none** — of the app's command-running paths it
//! was the only uncapped one, and the only one whose output goes straight into a
//! model's context window rather than into a scrollback panel a human scrolls.
//!
//! # Why this is not `background_shell`'s number
//!
//! There are two legitimate ceilings in this codebase and they differ by two
//! orders of magnitude, because they answer different questions:
//!
//! - **`background_shell::MAX_OUTPUT_BYTES` (256 KiB)** bounds a *human-facing*
//!   scrollback tail. Generous is correct there.
//! - **[`MODEL_OUTPUT_CAP`] (20 KB)** bounds what enters a *model's context*. At
//!   `contextTrimmer`'s own four-bytes-per-token estimate, 256 KiB is roughly
//!   65k tokens — one tool call would consume most of a typical local model's
//!   window.
//!
//! Reusing the smaller number rather than minting a third one is deliberate: the
//! consumer decides the ceiling, and `verify.rs` had already chosen this value
//! for exactly this consumer.

/// Bytes of each captured stream that may reach a model.
///
/// Per stream, not per call: stdout and stderr are capped independently, so the
/// worst case for one command is twice this.
pub const MODEL_OUTPUT_CAP: usize = 20_000;

/// The marker a truncated stream carries.
///
/// Shared so the two command runners are indistinguishable to a model — a
/// difference in wording would read as a difference in meaning.
pub const TRUNCATION_MARKER: &str = "… (truncated)\n";

/// Keeps the last `cap` bytes of `value`, returning whether anything was dropped.
///
/// **Tail, not head.** A failing command prints its diagnostic last: a compiler
/// emits thousands of progress lines and then the errors. The counter-case — a
/// command whose answer is its first line, like `ls` or `--help` — is short and
/// never reaches the cap, so keeping the tail costs nothing there.
///
/// Splits on a UTF-8 boundary, so a cut landing mid-codepoint widens the kept
/// region by a byte or two rather than panicking.
///
/// The `bool` exists because the marker alone is not a reliable signal: a command
/// is free to print the marker's text itself, and a caller that has to
/// `JSON.parse` the result needs to know whether it is looking at a whole
/// document or a fragment without pattern-matching on prose.
#[must_use]
pub fn cap_tail(value: String, cap: usize) -> (String, bool) {
    if value.len() <= cap {
        return (value, false);
    }
    let mut start = value.len() - cap;
    while start < value.len() && !value.is_char_boundary(start) {
        start += 1;
    }
    (format!("{TRUNCATION_MARKER}{}", &value[start..]), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_within_the_cap_is_untouched_and_not_reported_as_truncated() {
        let (value, truncated) = cap_tail("short".to_string(), MODEL_OUTPUT_CAP);
        assert_eq!(value, "short");
        assert!(!truncated);

        // Exactly at the cap is inside it, not over.
        let exact = "x".repeat(16);
        let (value, truncated) = cap_tail(exact.clone(), 16);
        assert_eq!(value, exact);
        assert!(!truncated);
    }

    #[test]
    fn a_capped_stream_keeps_its_tail_and_says_so() {
        let (value, truncated) = cap_tail("0123456789".to_string(), 4);
        assert!(truncated);
        assert_eq!(value, format!("{TRUNCATION_MARKER}6789"));
        assert!(
            value.ends_with("6789"),
            "the end of the output is what a failure puts its diagnostic in"
        );
    }

    /// The cut lands mid-codepoint here, which is what makes this worth its own
    /// test: naive slicing panics rather than returning a shorter string.
    #[test]
    fn a_cut_landing_inside_a_multibyte_character_does_not_panic() {
        // Four bytes each, so a cap of 5 cannot fall on a boundary.
        let value = "🙈🙉🙊".to_string();
        assert_eq!(value.len(), 12);
        let (capped, truncated) = cap_tail(value, 5);
        assert!(truncated);
        assert_eq!(
            capped,
            format!("{TRUNCATION_MARKER}🙊"),
            "the kept region must widen to the next boundary rather than split a char"
        );
    }

    /// The number is the point of this module, so it is asserted rather than left
    /// to a reader to infer from the constant's own definition.
    #[test]
    fn the_model_cap_is_the_smaller_of_the_two_ceilings_in_this_codebase() {
        assert_eq!(MODEL_OUTPUT_CAP, 20_000);
        assert!(
            MODEL_OUTPUT_CAP < crate::background_shell::MAX_OUTPUT_BYTES,
            "a model's context window is the tighter constraint of the two, not the looser"
        );
    }
}
