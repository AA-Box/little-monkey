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
//!
//! # Two shapes, and only one of them is a bound
//!
//! [`cap_tail`] trims a `String` that has already been collected. It is honest
//! about what reaches a *reader* and says nothing at all about what reached the
//! *heap*: a command that prints a gigabyte still allocated a gigabyte before
//! anything was trimmed, so the cap held the model's context window and not this
//! app's memory.
//!
//! [`CappedStream`] and [`drain_capped`] are the actual bound. Bytes are dropped
//! from the front as they arrive, so the retained buffer never exceeds the cap
//! however much the child produces. K4 asks for the second everywhere output is
//! captured; the first survives only for callers that already hold a whole string
//! from somewhere else.

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

/// One captured stream, held to its ceiling **while it is being produced**.
///
/// Bytes rather than a `String` because the cap is enforced during the read, when
/// a chunk boundary can land inside a multi-byte character and there is no whole
/// string to decode yet. [`Self::into_string`] does the one decode, at the end,
/// over a buffer that is bounded by construction.
///
/// Lifted out of `tools.rs`, which had the only implementation, because the two
/// other capture paths in this app — `verify.rs` and
/// `workspace_shell::run_to_output` — collected their streams whole and trimmed
/// afterwards. That is the failure K4 names explicitly: a bound applied after the
/// child exits bounds nothing about the app's memory while the child runs.
#[derive(Debug, Default)]
pub struct CappedStream {
    /// The kept tail. At most `cap` bytes once a cap is in force.
    bytes: Vec<u8>,
    /// Whether anything was dropped from the front to stay inside the cap.
    truncated: bool,
    /// Every byte the child produced, including the dropped ones.
    ///
    /// The cap is what the reader gets; this is what the *process* did, and an
    /// output limit has to be judged against the second. Without it a workload
    /// that printed a terabyte and a workload that printed 20 KB would be
    /// indistinguishable to the thing enforcing the output limit.
    total_bytes: u64,
}

impl CappedStream {
    /// Appends `chunk`, dropping whole bytes off the *front* once `cap` is
    /// exceeded.
    ///
    /// Tail, not head, for the reason [`cap_tail`] gives: a failing command
    /// prints its diagnostic last. Front-dropping is what makes this bounded in
    /// the first place — a head-keeping cap could stop reading, but then the
    /// child blocks forever on a full pipe instead of running to completion,
    /// which turns a noisy command into a timeout.
    pub fn push(&mut self, chunk: &[u8], cap: Option<usize>) {
        self.total_bytes = self.total_bytes.saturating_add(chunk.len() as u64);
        self.bytes.extend_from_slice(chunk);
        let Some(cap) = cap else { return };
        if self.bytes.len() <= cap {
            return;
        }
        let overflow = self.bytes.len() - cap;
        self.bytes.drain(..overflow);
        self.truncated = true;
    }

    /// How many bytes the child actually produced on this stream.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    #[must_use]
    pub fn was_truncated(&self) -> bool {
        self.truncated
    }

    /// The retained bytes, for a caller that wants them undecoded.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Decodes the kept tail, prefixing the shared truncation marker if the front
    /// was dropped.
    ///
    /// The leading continuation bytes are shed first, so a cut that landed inside
    /// a character widens the kept region forward to the next boundary instead of
    /// decoding to a replacement character. That is exactly what [`cap_tail`] does
    /// when it works on a whole `String`, and keeping the behaviour identical is
    /// why it is done here rather than left to `from_utf8_lossy`.
    #[must_use]
    pub fn into_string(mut self) -> (String, bool) {
        if self.truncated {
            let keep = self
                .bytes
                .iter()
                .position(|byte| (byte & 0b1100_0000) != 0b1000_0000)
                .unwrap_or(self.bytes.len());
            self.bytes.drain(..keep);
        }
        let decoded = String::from_utf8_lossy(&self.bytes).to_string();
        if self.truncated {
            (format!("{TRUNCATION_MARKER}{decoded}"), true)
        } else {
            (decoded, false)
        }
    }
}

/// Reads one pipe to EOF, keeping at most `cap` bytes of its tail.
///
/// Reading to EOF rather than stopping at the cap is the whole point: an
/// early-returning reader leaves the child blocked on a full pipe buffer, so a
/// command that merely printed too much would report a timeout instead of its
/// exit code. Callers must drain stdout and stderr *concurrently* for the same
/// reason — a child that fills stderr while this awaits stdout deadlocks.
pub async fn drain_capped<R>(mut reader: R, cap: Option<usize>) -> std::io::Result<CappedStream>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut stream = CappedStream::default();
    // 8 KiB: larger than the 64 KiB pipe buffer would not help (the kernel hands
    // over what it has), and smaller would just mean more syscalls.
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok(stream);
        }
        stream.push(&chunk[..read], cap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the whole type exists for: retained memory is bounded by the
    /// cap, not by what the child chose to print.
    #[test]
    fn a_flood_far_past_the_cap_never_grows_the_retained_buffer() {
        let mut stream = CappedStream::default();
        for _ in 0..1_000 {
            stream.push(&[b'x'; 8 * 1024], Some(1_024));
            assert!(
                stream.as_bytes().len() <= 1_024,
                "the buffer grew past the cap mid-stream, which is the bound this type is"
            );
        }
        assert!(stream.was_truncated());
        assert_eq!(
            stream.total_bytes(),
            1_000 * 8 * 1024,
            "the cap bounds what is kept; an output limit is judged against what was produced"
        );
    }

    #[test]
    fn a_stream_inside_the_cap_keeps_everything_and_reports_no_truncation() {
        let mut stream = CappedStream::default();
        stream.push(b"hello", Some(1_024));
        assert_eq!(stream.total_bytes(), 5);
        assert_eq!(stream.into_string(), ("hello".to_string(), false));
    }

    #[test]
    fn a_cut_landing_inside_a_multibyte_character_widens_rather_than_corrupts() {
        let mut stream = CappedStream::default();
        // Four bytes each, so a cap of 5 cannot fall on a boundary.
        stream.push("🙈🙉🙊".as_bytes(), Some(5));
        let (text, truncated) = stream.into_string();
        assert!(truncated);
        assert_eq!(text, format!("{TRUNCATION_MARKER}🙊"));
    }

    #[test]
    fn no_cap_means_no_truncation() {
        let mut stream = CappedStream::default();
        stream.push(&[b'x'; 100_000], None);
        assert!(!stream.was_truncated());
        assert_eq!(stream.as_bytes().len(), 100_000);
    }

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
