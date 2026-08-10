//! Measured benchmarking — ROADMAP #2, agent-os roadmap K6(a).
//!
//! The one rule this module exists to enforce is ROADMAP #2's own sentence:
//! **"No number is displayed that was not measured on the machine displaying
//! it."** Every duration here comes from [`Instant`] in this process; every
//! token count comes from the runtime's own `ResponseCompleted` usage; peak
//! memory comes from [`crate::process_usage::sample`] against the runtime's
//! real pid. Nothing is derived from a model name, a file size, or a table of
//! device classes — which is what the surface this replaces did (see
//! `runtimeEdgeProfiles.ts`, whose prose defers to "the local benchmark" that
//! until now did not exist).
//!
//! The same discipline as K6(b)'s resource ledger applies to gaps: an
//! unmeasurable field is `None` *and* carries a [`TraceFieldNote`] saying why.
//! There is no zero-as-unknown anywhere in this module, because a benchmark
//! that reports `0.0 tok/s` for a failed run is worse than one that reports
//! nothing.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::compatibility_hub::{CanonicalStreamEvent, CanonicalUsage};
use crate::m3_runtime_hub::M3CanonicalStreamSink;
use crate::process_usage::{self, FIELD_PEAK_RSS_BYTES};
use crate::runtime_telemetry::TraceFieldNote;

/// Field names a [`TraceFieldNote`] may name, spelled once. camelCase to match
/// the wire shape the panel reads, the same convention `process_usage` uses.
pub const FIELD_TTFT_MS: &str = "timeToFirstTokenMs";
pub const FIELD_DECODE_MS: &str = "decodeMs";
pub const FIELD_DECODE_TOKENS_PER_SECOND: &str = "decodeTokensPerSecond";
pub const FIELD_OUTPUT_TOKENS: &str = "outputTokens";
/// This *run's* peak, as distinct from the process lifetime high-water mark
/// that [`process_usage`] reports — see [`PeakMemory`].
pub const FIELD_RUN_PEAK_RSS_BYTES: &str = "runPeakRssBytes";

/// A sink that measures a generation and keeps none of it.
///
/// Structurally the third canonical-sink decorator in this tree, after
/// `ProtocolEncodingSink` and `MlxCanonicalSink` — except that it has no
/// downstream, because a benchmark has no consumer for the text. It records
/// four instants and the runtime's own usage, and drops every payload.
///
/// The first *non-empty* [`CanonicalStreamEvent::TextDelta`] is what stamps
/// time-to-first-token. Not `ResponseStart`, which a runtime may emit before it
/// has done any prefill work at all, and not `TextStart`, which announces a
/// content block rather than content. An empty delta is skipped for the same
/// reason: it carries no token.
pub struct TimingSink {
    started_at: Instant,
    first_text_at: Option<Instant>,
    completed_at: Option<Instant>,
    usage: Option<CanonicalUsage>,
    /// The runtime's own error event. A benchmark sample that errored is
    /// reported as failed, never folded into the statistics as a slow one.
    error: Option<String>,
}

impl TimingSink {
    /// Starts the clock. Construct this immediately before calling the
    /// runtime's `stream`, since everything between the two is charged to
    /// time-to-first-token.
    pub fn started_now() -> Self {
        Self {
            started_at: Instant::now(),
            first_text_at: None,
            completed_at: None,
            usage: None,
            error: None,
        }
    }

    /// The runtime's error, if it reported one.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Converts what was observed into a sample. `total_ms` is measured to the
    /// completion event where there was one, and to now otherwise, so a stream
    /// that ended without completing still reports the time it consumed.
    pub fn finish(self) -> SampleTimings {
        let ended_at = self.completed_at.unwrap_or_else(Instant::now);
        let mut unavailable = Vec::new();

        let time_to_first_token_ms = match self.first_text_at {
            Some(at) => Some(elapsed_ms(self.started_at, at)),
            None => {
                unavailable.push(TraceFieldNote {
                    field: FIELD_TTFT_MS.to_string(),
                    reason: "the stream produced no text delta, so no first token was observed"
                        .to_string(),
                });
                None
            }
        };
        let decode_ms = match self.first_text_at {
            Some(at) => Some(elapsed_ms(at, ended_at)),
            None => {
                unavailable.push(TraceFieldNote {
                    field: FIELD_DECODE_MS.to_string(),
                    reason: "the decode window opens at the first token, which never arrived"
                        .to_string(),
                });
                None
            }
        };
        let (input_tokens, output_tokens) = match &self.usage {
            Some(usage) => (Some(usage.input_tokens), Some(usage.output_tokens)),
            None => {
                unavailable.push(TraceFieldNote {
                    field: FIELD_OUTPUT_TOKENS.to_string(),
                    reason: "the runtime completed without reporting usage, and a token count \
                             inferred from the text would not be the runtime's own tokenization"
                        .to_string(),
                });
                (None, None)
            }
        };

        SampleTimings {
            total_ms: elapsed_ms(self.started_at, ended_at),
            time_to_first_token_ms,
            decode_ms,
            input_tokens,
            output_tokens,
            error: self.error,
            unavailable,
        }
    }
}

impl M3CanonicalStreamSink for TimingSink {
    fn emit(&mut self, event: CanonicalStreamEvent) -> Result<(), String> {
        match event {
            CanonicalStreamEvent::TextDelta { text, .. } if !text.is_empty() => {
                self.first_text_at.get_or_insert_with(Instant::now);
            }
            CanonicalStreamEvent::ResponseCompleted { usage, .. } => {
                self.completed_at.get_or_insert_with(Instant::now);
                self.usage = Some(usage);
            }
            CanonicalStreamEvent::Error { message, .. } => {
                self.completed_at.get_or_insert_with(Instant::now);
                self.error = Some(message);
            }
            _ => {}
        }
        Ok(())
    }
}

/// Saturating whole milliseconds between two instants. `Instant` is monotonic,
/// so unlike the `Date.now()` deltas the telemetry traces are built from, this
/// cannot go backwards when the wall clock is adjusted.
fn elapsed_ms(from: Instant, to: Instant) -> u64 {
    to.saturating_duration_since(from).as_millis() as u64
}

/// One timed generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SampleTimings {
    pub total_ms: u64,
    pub time_to_first_token_ms: Option<u64>,
    /// First token to completion.
    pub decode_ms: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Set when the runtime emitted an error event. A sample with an error is
    /// excluded from every statistic.
    pub error: Option<String>,
    pub unavailable: Vec<TraceFieldNote>,
}

impl SampleTimings {
    /// Records a failure the stream itself did not report as an `Error` event —
    /// a driver that returned `Err` after emitting some text, for instance.
    /// Never overwrites the runtime's own message, which is the more specific of
    /// the two.
    pub fn record_error(&mut self, message: String) {
        if self.error.is_none() {
            self.error = Some(message);
        }
    }

    /// True when this sample may contribute to the reported statistics.
    pub fn is_usable(&self) -> bool {
        self.error.is_none() && self.time_to_first_token_ms.is_some()
    }

    /// Decode throughput, in tokens per second.
    ///
    /// The numerator is `output_tokens - 1`, not `output_tokens`, and the
    /// difference is not a rounding detail: the first token's cost is already
    /// reported as time-to-first-token, so charging it to the decode window
    /// too counts it twice. At a 128-token budget that is a ~0.8% overstatement
    /// — small, plausible, and exactly the sort of quiet inaccuracy that
    /// survives review forever.
    ///
    /// `None` rather than `0.0` when the window holds no tokens to divide by,
    /// or when it rounded to zero milliseconds.
    pub fn decode_tokens_per_second(&self) -> Result<f64, TraceFieldNote> {
        let note = |reason: &str| TraceFieldNote {
            field: FIELD_DECODE_TOKENS_PER_SECOND.to_string(),
            reason: reason.to_string(),
        };
        let output = self
            .output_tokens
            .ok_or_else(|| note("the runtime reported no output token count to divide"))?;
        let decode_ms = self
            .decode_ms
            .ok_or_else(|| note("there is no decode window without a first token"))?;
        let decoded_after_first = output
            .checked_sub(1)
            .filter(|count| *count > 0)
            .ok_or_else(|| {
                note("only one token was produced, so no token fell inside the decode window")
            })?;
        if decode_ms == 0 {
            return Err(note(
                "the decode window rounded to zero milliseconds, which bounds the rate rather \
                 than measuring it",
            ));
        }
        Ok(decoded_after_first as f64 / (decode_ms as f64 / 1_000.0))
    }
}

/// What can honestly be said about memory after a run.
///
/// [`crate::process_usage::sample`] reports the **process lifetime** high-water
/// mark, which is the right number for a process the ledger owns end to end and
/// the wrong one here: a model server that has been resident for an hour already
/// carries a peak set by somebody else's request. So both readings are taken and
/// the distinction is kept rather than collapsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PeakMemory {
    /// The process lifetime high-water mark after the run. An upper bound on
    /// this run's peak, and only that.
    pub process_lifetime_peak_bytes: Option<u64>,
    /// The same mark read before the run started.
    pub before_bytes: Option<u64>,
    /// Set only when the mark actually rose across the run — the one case where
    /// the lifetime peak *is* this run's peak, because this run is what set it.
    pub run_peak_bytes: Option<u64>,
    pub unavailable: Vec<TraceFieldNote>,
}

impl PeakMemory {
    /// Reads the mark around a run. `pid` is `None` for a runtime this machine
    /// does not host — a remote OpenAI-compatible endpoint has no local process
    /// whose memory could be sampled, and saying so is the honest answer.
    pub fn measure(pid: Option<i64>, before: Option<u64>, after: Option<u64>) -> Self {
        let mut unavailable = Vec::new();
        let Some(pid) = pid else {
            unavailable.push(TraceFieldNote {
                field: FIELD_RUN_PEAK_RSS_BYTES.to_string(),
                reason: "this runtime does not host a local process, so no memory of its own is \
                         measurable from this machine"
                    .to_string(),
            });
            return Self {
                process_lifetime_peak_bytes: None,
                before_bytes: None,
                run_peak_bytes: None,
                unavailable,
            };
        };

        let run_peak_bytes = match (before, after) {
            (Some(before), Some(after)) if after > before => Some(after),
            (_, Some(after)) => {
                unavailable.push(TraceFieldNote {
                    field: FIELD_RUN_PEAK_RSS_BYTES.to_string(),
                    reason: format!(
                        "pid {pid}'s high-water mark did not rise during this run, so its peak of \
                         {after} bytes was set earlier and bounds this run's peak rather than \
                         reporting it"
                    ),
                });
                None
            }
            (_, None) => None,
        };
        Self {
            process_lifetime_peak_bytes: after,
            before_bytes: before,
            run_peak_bytes,
            unavailable,
        }
    }

    /// Samples `pid`'s current high-water mark, carrying the platform's own
    /// reason forward when it has none to give. Windows is the live case: the
    /// windows-sys feature modules this crate enables do not cover peak working
    /// set, so this is `None` with that reason there.
    pub fn sample_mark(pid: i64) -> (Option<u64>, Option<TraceFieldNote>) {
        let sample = process_usage::sample(pid);
        match sample.peak_rss_bytes {
            Some(bytes) => (Some(bytes), None),
            None => (
                None,
                sample.note_for(FIELD_PEAK_RSS_BYTES).cloned().or_else(|| {
                    Some(TraceFieldNote {
                        field: FIELD_RUN_PEAK_RSS_BYTES.to_string(),
                        reason: "this platform reported no peak resident size for the runtime \
                                 process"
                            .to_string(),
                    })
                }),
            ),
        }
    }
}

/// The spread of one measured quantity across repeats.
///
/// `stddev` is the **sample** standard deviation, and it is `None` for a single
/// repeat rather than `0.0`: one observation has no spread to report, and zero
/// would read as "perfectly repeatable".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Spread {
    pub n: u32,
    pub min: f64,
    pub median: f64,
    pub max: f64,
    pub stddev: Option<f64>,
}

/// `None` for an empty slice — there is no spread of nothing.
pub fn spread(values: &[f64]) -> Option<Spread> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    let median = if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    };
    let mean = sorted.iter().sum::<f64>() / n as f64;
    // Bessel's correction: dividing by n would understate the spread of a
    // sample, and a benchmark's repeats are a sample of this machine's
    // behaviour rather than its population.
    let stddev = if n > 1 {
        let variance = sorted
            .iter()
            .map(|value| (value - mean) * (value - mean))
            .sum::<f64>()
            / (n - 1) as f64;
        Some(variance.sqrt())
    } else {
        None
    };
    Some(Spread {
        n: n as u32,
        min: sorted[0],
        median,
        max: sorted[n - 1],
        stddev,
    })
}

/// Bumped whenever a stored report's shape changes, so a persisted report from
/// an older build is discarded rather than misread.
pub const BENCHMARK_SCHEMA_VERSION: u32 = 1;

/// The prompt a run uses when the caller names none.
///
/// Lives here rather than in the panel so two runs are comparable across app
/// versions: a prompt that drifts changes the prefill work being timed, and
/// time-to-first-token is largely a measurement of prefill.
pub const DEFAULT_BENCHMARK_PROMPT: &str =
    "Write a short paragraph explaining what a benchmark measures.";

/// Below this many output tokens a decode rate is noise rather than a
/// measurement. The abandoned prototype's headline figure came from a task
/// prompted with "Reply with exactly the word OK." — two tokens — and it was
/// the number rendered in its UI.
pub const MIN_OUTPUT_TOKENS: u32 = 32;
pub const MAX_OUTPUT_TOKENS: u32 = 2_048;
/// One repeat would be consumed entirely by the warm-up, leaving nothing
/// counted, so two is the real floor.
pub const MIN_REPEATS: u32 = 2;
pub const MAX_REPEATS: u32 = 20;
/// Leading repeats excluded from the statistics. One is enough to pay for
/// loading the weights and filling caches; more would cost the user minutes to
/// buy very little.
pub const WARMUP_REPEATS: u32 = 1;

/// What to measure. Validated at the command boundary by [`Self::validated`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BenchmarkRequest {
    pub runtime_id: String,
    pub model: String,
    /// `None` uses [`DEFAULT_BENCHMARK_PROMPT`].
    pub prompt: Option<String>,
    pub max_output_tokens: u32,
    pub repeats: u32,
}

impl BenchmarkRequest {
    /// Rejects a request that could only produce a number too weak to report.
    /// The bounds are refusals rather than clamps on purpose: silently running
    /// 32 tokens when the caller asked for 2 would return a report whose
    /// `maxOutputTokens` disagreed with what was requested.
    pub fn validated(self) -> Result<Self, String> {
        if self.runtime_id.trim().is_empty() {
            return Err("A benchmark needs a runtime to measure.".to_string());
        }
        if self.model.trim().is_empty() {
            return Err("A benchmark needs a model to measure.".to_string());
        }
        if !(MIN_OUTPUT_TOKENS..=MAX_OUTPUT_TOKENS).contains(&self.max_output_tokens) {
            return Err(format!(
                "Generate between {MIN_OUTPUT_TOKENS} and {MAX_OUTPUT_TOKENS} tokens per repeat. \
                 Fewer than {MIN_OUTPUT_TOKENS} makes the decode rate noise rather than a \
                 measurement."
            ));
        }
        if !(MIN_REPEATS..=MAX_REPEATS).contains(&self.repeats) {
            return Err(format!(
                "Run between {MIN_REPEATS} and {MAX_REPEATS} repeats. The first is discarded as \
                 warm-up, so a single repeat would leave nothing counted."
            ));
        }
        Ok(self)
    }

    /// The prompt this run will send.
    pub fn prompt_text(&self) -> &str {
        match self.prompt.as_deref().map(str::trim) {
            Some(prompt) if !prompt.is_empty() => prompt,
            _ => DEFAULT_BENCHMARK_PROMPT,
        }
    }
}

/// What one repeat contributed, in the shape the panel reads. `decodeTokensPerSecond`
/// is flattened out of [`SampleTimings::decode_tokens_per_second`] here so the
/// frontend never recomputes a rate — the number it renders is the number this
/// machine measured, not an arithmetic it performed on two other fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SampleReport {
    /// 1-based, counting the discarded warm-up, so a reader can see which
    /// repeat a number came from.
    pub repeat: u32,
    pub warmup: bool,
    pub timings: SampleTimings,
    pub decode_tokens_per_second: Option<f64>,
}

/// The parts of a hardware snapshot a stored benchmark is only valid *for*.
///
/// A whole [`crate::runtime_adapter::HardwareSnapshot`] cannot be compared: it
/// carries `captured_at_ms` and `available_ram_bytes`, both of which change
/// second to second, so equality on it would call every stored report stale.
/// This is the stable identity instead — swap the machine, add a GPU, or change
/// how much RAM is fitted, and a number measured before that is no longer a
/// number measured on the machine displaying it.
///
/// Deliberately **not** including `supported_runtimes`: it is derived from
/// `os`/`arch`, so comparing it would only ever restate them, and a build that
/// learns to support a new runtime would invalidate every stored report for no
/// physical reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MachineIdentity {
    pub os: String,
    pub arch: String,
    pub total_ram_bytes: u64,
    pub logical_cpu_count: u32,
    /// Sorted, so two snapshots that found the same devices in a different order
    /// are the same machine.
    pub accelerators: Vec<String>,
}

/// Why a stored benchmark does or does not describe the machine asking.
///
/// A tagged union rather than a `bool` plus a list, for the reason
/// [`crate::run_ledger::ChainVerification`] is one: a caller must not be able to
/// read the differences off a report and still present its numbers as this
/// machine's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum BenchmarkFreshness {
    /// Measured on the machine now asking. Its numbers may be displayed.
    ThisMachine,
    /// Measured somewhere else, or here before the hardware changed. Every
    /// difference is named, and no number from it may be displayed as current.
    DifferentMachine { changed: Vec<String> },
}

impl MachineIdentity {
    /// Compare against `other`, naming every difference.
    ///
    /// The differences are for a human to read, so they say what changed rather
    /// than dumping both structs.
    #[must_use]
    pub fn freshness_against(&self, other: &MachineIdentity) -> BenchmarkFreshness {
        let mut changed = Vec::new();
        if self.os != other.os || self.arch != other.arch {
            changed.push(format!(
                "platform {}/{} → {}/{}",
                self.os, self.arch, other.os, other.arch
            ));
        }
        if self.total_ram_bytes != other.total_ram_bytes {
            changed.push(format!(
                "installed RAM {} → {} bytes",
                self.total_ram_bytes, other.total_ram_bytes
            ));
        }
        if self.logical_cpu_count != other.logical_cpu_count {
            changed.push(format!(
                "logical CPUs {} → {}",
                self.logical_cpu_count, other.logical_cpu_count
            ));
        }
        if self.accelerators != other.accelerators {
            changed.push(format!(
                "accelerators [{}] → [{}]",
                self.accelerators.join(", "),
                other.accelerators.join(", ")
            ));
        }
        if changed.is_empty() {
            BenchmarkFreshness::ThisMachine
        } else {
            BenchmarkFreshness::DifferentMachine { changed }
        }
    }
}

/// One benchmark run of one model on one runtime on this machine.
///
/// The hardware snapshot is deliberately not stored here: the caller attaches
/// it, because this module must not be able to invent one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BenchmarkReport {
    pub schema_version: u32,
    pub runtime_id: String,
    pub model: String,
    /// The runtime's own reported quantization. `None` with a note when the
    /// runtime does not report one — never guessed from the model's name, which
    /// is where "Q4_K_M" in a filename would tempt a reader.
    pub quantization: Option<String>,
    pub max_output_tokens: u32,
    pub repeats_requested: u32,
    /// How many leading repeats were excluded. A cold first request pays for
    /// loading weights, and charging that to time-to-first-token would report
    /// load time as prefill.
    pub warmup_discarded: u32,
    pub samples: Vec<SampleReport>,
    pub time_to_first_token_ms: Option<Spread>,
    pub decode_tokens_per_second: Option<Spread>,
    pub peak_memory: PeakMemory,
    pub unavailable: Vec<TraceFieldNote>,
}

/// Folds the per-repeat samples into the report.
///
/// `samples` is in execution order and includes the warm-up repeats, which are
/// reported but never counted. Every statistic is computed only from samples
/// that [`SampleTimings::is_usable`] accepts, so an errored repeat narrows `n`
/// rather than dragging a median down.
/// Takes the whole `request` rather than its fields one by one, which is what
/// makes the report structurally unable to disagree with what was asked for.
/// `validated` refuses out-of-range input instead of clamping it precisely so
/// that `maxOutputTokens` in the report is the number the caller sent; passing
/// the field separately would leave room to break that by hand.
pub fn summarize(
    request: &BenchmarkRequest,
    quantization: Option<String>,
    warmup_discarded: u32,
    samples: Vec<SampleTimings>,
    peak_memory: PeakMemory,
) -> BenchmarkReport {
    let mut unavailable = Vec::new();
    let mut reported = Vec::with_capacity(samples.len());
    let mut ttft_values = Vec::new();
    let mut rate_values = Vec::new();

    for (index, timings) in samples.into_iter().enumerate() {
        let warmup = (index as u32) < warmup_discarded;
        let rate = match timings.decode_tokens_per_second() {
            Ok(rate) => Some(rate),
            Err(note) => {
                // One note per distinct reason, not one per repeat: five repeats
                // failing the same way is one fact about this runtime.
                if !warmup && !unavailable.contains(&note) {
                    unavailable.push(note);
                }
                None
            }
        };
        if !warmup && timings.is_usable() {
            if let Some(ms) = timings.time_to_first_token_ms {
                ttft_values.push(ms as f64);
            }
            if let Some(rate) = rate {
                rate_values.push(rate);
            }
        }
        reported.push(SampleReport {
            repeat: index as u32 + 1,
            warmup,
            timings,
            decode_tokens_per_second: rate,
        });
    }

    if reported.len() as u32 <= warmup_discarded {
        unavailable.push(TraceFieldNote {
            field: FIELD_TTFT_MS.to_string(),
            reason: "every repeat was a discarded warm-up, so nothing was left to measure"
                .to_string(),
        });
    }

    BenchmarkReport {
        schema_version: BENCHMARK_SCHEMA_VERSION,
        runtime_id: request.runtime_id.clone(),
        model: request.model.clone(),
        quantization,
        max_output_tokens: request.max_output_tokens,
        repeats_requested: request.repeats,
        warmup_discarded,
        samples: reported,
        time_to_first_token_ms: spread(&ttft_values),
        decode_tokens_per_second: spread(&rate_values),
        peak_memory,
        unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(text: &str) -> CanonicalStreamEvent {
        CanonicalStreamEvent::TextDelta {
            index: 0,
            text: text.to_string(),
        }
    }

    fn completed(output_tokens: u64) -> CanonicalStreamEvent {
        CanonicalStreamEvent::ResponseCompleted {
            response_id: "response-1".to_string(),
            finish_reason: "stop".to_string(),
            usage: CanonicalUsage {
                input_tokens: 7,
                output_tokens,
                cached_input_tokens: None,
            },
        }
    }

    /// The whole point of the sink: the clock for time-to-first-token starts at
    /// construction and stops at the first delta that carries text, not at the
    /// events a runtime may emit before it has generated anything.
    #[test]
    fn first_token_is_the_first_non_empty_text_delta() {
        let mut sink = TimingSink::started_now();
        sink.emit(CanonicalStreamEvent::ResponseStart {
            response_id: "response-1".to_string(),
            model: "m".to_string(),
            created_at_seconds: 0,
        })
        .unwrap();
        sink.emit(CanonicalStreamEvent::TextStart { index: 0 })
            .unwrap();
        sink.emit(delta("")).unwrap();
        assert!(
            sink.first_text_at.is_none(),
            "an empty delta carries no token, so it cannot be the first one"
        );
        std::thread::sleep(std::time::Duration::from_millis(12));
        sink.emit(delta("Hel")).unwrap();
        let first = sink.first_text_at.expect("the first real delta stamps it");
        sink.emit(delta("lo")).unwrap();
        assert_eq!(
            sink.first_text_at.expect("still set"),
            first,
            "a later delta must not move the first-token instant"
        );

        sink.emit(completed(4)).unwrap();
        let sample = sink.finish();
        assert!(
            sample.time_to_first_token_ms >= Some(12),
            "time-to-first-token must cover the sleep before the first delta, got {:?}",
            sample.time_to_first_token_ms
        );
        assert_eq!(sample.output_tokens, Some(4));
        assert!(
            sample.unavailable.is_empty(),
            "nothing was unmeasurable here"
        );
        assert!(sample.is_usable());
    }

    /// A stream that errors is reported as failed. The regression this guards is
    /// the one the abandoned prototype shipped: a failed task recorded
    /// `tokensPerSecond: 0` and rendered as "0.0 tok/s", indistinguishable from a
    /// genuine measurement of a very slow model.
    #[test]
    fn an_errored_stream_is_unusable_rather_than_zero() {
        let mut sink = TimingSink::started_now();
        sink.emit(delta("part")).unwrap();
        sink.emit(CanonicalStreamEvent::Error {
            code: "runtime".to_string(),
            message: "backend died".to_string(),
            retryable: false,
        })
        .unwrap();
        let sample = sink.finish();
        assert_eq!(sample.error.as_deref(), Some("backend died"));
        assert!(
            !sample.is_usable(),
            "an errored sample cannot enter a statistic"
        );
        assert!(
            sample.decode_tokens_per_second().is_err(),
            "and it has no rate, rather than a rate of zero"
        );
    }

    /// A note's `field` must be the name the field actually serializes under, or
    /// the panel looks up a reason that is not there and renders a fallback
    /// sentence instead of the real one. `process_usage`'s field consts exist for
    /// the same reason — "so a note and the field it explains are linkable
    /// without a mapping table" — and this pins it for a `Serialize` derive,
    /// which is where the two can silently drift apart.
    #[test]
    fn every_field_note_names_a_field_that_actually_serializes_under_that_name() {
        let json = serde_json::to_value(SampleTimings {
            total_ms: 1,
            time_to_first_token_ms: None,
            decode_ms: None,
            input_tokens: None,
            output_tokens: None,
            error: None,
            unavailable: Vec::new(),
        })
        .expect("a sample serializes");
        let object = json.as_object().expect("an object");
        for field in [FIELD_TTFT_MS, FIELD_DECODE_MS, FIELD_OUTPUT_TOKENS] {
            assert!(
                object.contains_key(field),
                "a note names {field}, but the wire shape has {:?}",
                object.keys().collect::<Vec<_>>()
            );
        }
    }

    /// No text at all means no first token, and therefore no decode window —
    /// both stated as reasons rather than left as bare `None`s.
    #[test]
    fn a_stream_with_no_text_states_why_each_field_is_missing() {
        let mut sink = TimingSink::started_now();
        sink.emit(completed(0)).unwrap();
        let sample = sink.finish();
        assert_eq!(sample.time_to_first_token_ms, None);
        assert_eq!(sample.decode_ms, None);
        let named: Vec<&str> = sample
            .unavailable
            .iter()
            .map(|note| note.field.as_str())
            .collect();
        assert_eq!(named, vec![FIELD_TTFT_MS, FIELD_DECODE_MS]);
        assert!(
            sample
                .unavailable
                .iter()
                .all(|note| !note.reason.is_empty()),
            "every gap names a reason"
        );
        assert!(!sample.is_usable());
    }

    /// The first token is charged to time-to-first-token, so the decode window's
    /// numerator excludes it.
    #[test]
    fn decode_rate_excludes_the_token_already_charged_to_ttft() {
        let sample = SampleTimings {
            total_ms: 1_100,
            time_to_first_token_ms: Some(100),
            decode_ms: Some(1_000),
            input_tokens: Some(7),
            output_tokens: Some(101),
            error: None,
            unavailable: Vec::new(),
        };
        assert_eq!(
            sample.decode_tokens_per_second().unwrap(),
            100.0,
            "101 output tokens over a 1s window is 100 decoded after the first, not 101"
        );
    }

    /// Every way the rate can fail to exist returns a reason, and none of them
    /// returns zero.
    #[test]
    fn a_rate_that_cannot_be_computed_says_so_instead_of_returning_zero() {
        let base = SampleTimings {
            total_ms: 10,
            time_to_first_token_ms: Some(5),
            decode_ms: Some(5),
            input_tokens: Some(1),
            output_tokens: Some(9),
            error: None,
            unavailable: Vec::new(),
        };

        let single_token = SampleTimings {
            output_tokens: Some(1),
            ..base.clone()
        };
        let note = single_token
            .decode_tokens_per_second()
            .expect_err("no rate");
        assert_eq!(note.field, FIELD_DECODE_TOKENS_PER_SECOND);
        assert!(note.reason.contains("one token"), "got {}", note.reason);

        let instant_decode = SampleTimings {
            decode_ms: Some(0),
            ..base.clone()
        };
        assert!(instant_decode
            .decode_tokens_per_second()
            .expect_err("no rate")
            .reason
            .contains("zero milliseconds"),);

        let no_usage = SampleTimings {
            output_tokens: None,
            ..base
        };
        assert!(no_usage.decode_tokens_per_second().is_err());
    }

    /// A lifetime high-water mark that did not move during the run bounds this
    /// run's peak; it does not report it. Collapsing the two would attribute an
    /// earlier request's memory to this benchmark.
    #[test]
    fn peak_memory_separates_this_runs_peak_from_the_process_lifetime_mark() {
        let rose = PeakMemory::measure(Some(4_242), Some(1_000), Some(2_500));
        assert_eq!(rose.run_peak_bytes, Some(2_500));
        assert!(rose.unavailable.is_empty());

        let flat = PeakMemory::measure(Some(4_242), Some(9_000), Some(9_000));
        assert_eq!(flat.run_peak_bytes, None, "this run did not set the mark");
        assert_eq!(
            flat.process_lifetime_peak_bytes,
            Some(9_000),
            "the bound is still worth reporting"
        );
        assert_eq!(flat.unavailable.len(), 1);
        assert!(flat.unavailable[0].reason.contains("set earlier"));

        let remote = PeakMemory::measure(None, None, None);
        assert_eq!(remote.run_peak_bytes, None);
        assert_eq!(remote.process_lifetime_peak_bytes, None);
        assert!(
            remote.unavailable[0]
                .reason
                .contains("does not host a local process"),
            "a remote endpoint has no memory of its own to measure"
        );
    }

    fn request() -> BenchmarkRequest {
        BenchmarkRequest {
            runtime_id: "managed-llama".to_string(),
            model: "m".to_string(),
            prompt: None,
            max_output_tokens: 128,
            repeats: 5,
        }
    }

    /// The bounds refuse rather than clamp, so a report's `maxOutputTokens` can
    /// never disagree with what the caller asked for.
    #[test]
    fn a_request_too_small_to_measure_is_refused_not_clamped() {
        let two_tokens = BenchmarkRequest {
            max_output_tokens: 2,
            ..request()
        };
        let error = two_tokens.validated().expect_err("two tokens is noise");
        assert!(error.contains("noise"), "got {error}");

        let single = BenchmarkRequest {
            repeats: 1,
            ..request()
        };
        assert!(single
            .validated()
            .expect_err("one repeat is all warm-up")
            .contains("warm-up"),);

        assert!(BenchmarkRequest {
            runtime_id: "  ".to_string(),
            ..request()
        }
        .validated()
        .is_err());
        assert!(request().validated().is_ok());
    }

    /// A blank prompt falls back to the shared default rather than timing the
    /// prefill of an empty string.
    #[test]
    fn a_blank_prompt_falls_back_to_the_shared_default() {
        assert_eq!(request().prompt_text(), DEFAULT_BENCHMARK_PROMPT);
        assert_eq!(
            BenchmarkRequest {
                prompt: Some("   ".to_string()),
                ..request()
            }
            .prompt_text(),
            DEFAULT_BENCHMARK_PROMPT
        );
        assert_eq!(
            BenchmarkRequest {
                prompt: Some("count to ten".to_string()),
                ..request()
            }
            .prompt_text(),
            "count to ten"
        );
    }

    fn usable(time_to_first_token_ms: u64, decode_ms: u64, output_tokens: u64) -> SampleTimings {
        SampleTimings {
            total_ms: time_to_first_token_ms + decode_ms,
            time_to_first_token_ms: Some(time_to_first_token_ms),
            decode_ms: Some(decode_ms),
            input_tokens: Some(7),
            output_tokens: Some(output_tokens),
            error: None,
            unavailable: Vec::new(),
        }
    }

    /// The warm-up is reported but never counted. The cold first request pays to
    /// load the weights, so counting it would report load time as prefill — and
    /// it is the confound that made the abandoned prototype's headline number
    /// meaningless, since its one un-repeated task was also its cold one.
    #[test]
    fn the_warmup_repeat_is_reported_but_excluded_from_every_statistic() {
        let report = summarize(
            &BenchmarkRequest {
                repeats: 3,
                ..request()
            },
            Some("Q4_K_M".to_string()),
            1,
            vec![
                usable(5_000, 1_000, 101), // cold: 5s to first token
                usable(100, 1_000, 101),
                usable(300, 1_000, 101),
            ],
            PeakMemory::measure(Some(11), Some(1_000), Some(4_000)),
        );

        assert_eq!(report.samples.len(), 3, "all three repeats are visible");
        assert!(report.samples[0].warmup);
        assert!(!report.samples[1].warmup);
        assert_eq!(
            report.samples[0].timings.time_to_first_token_ms,
            Some(5_000),
            "the discarded repeat's own numbers are still reported"
        );

        let ttft = report.time_to_first_token_ms.expect("two counted repeats");
        assert_eq!(
            (ttft.n, ttft.min, ttft.max),
            (2, 100.0, 300.0),
            "the 5s cold repeat must not appear in the spread"
        );
        assert_eq!(ttft.median, 200.0);
        let rate = report
            .decode_tokens_per_second
            .expect("two counted repeats");
        assert_eq!((rate.n, rate.min, rate.max), (2, 100.0, 100.0));
        assert_eq!(
            rate.stddev,
            Some(0.0),
            "two identical rates genuinely have no spread"
        );
        assert_eq!(report.peak_memory.run_peak_bytes, Some(4_000));
        assert!(report.unavailable.is_empty());
    }

    /// An errored repeat narrows `n`. It must not enter the statistics as a slow
    /// sample, and the reason must survive into the report exactly once however
    /// many repeats failed the same way.
    #[test]
    fn an_errored_repeat_narrows_n_and_states_its_reason_once() {
        let failed = SampleTimings {
            total_ms: 40,
            time_to_first_token_ms: None,
            decode_ms: None,
            input_tokens: None,
            output_tokens: None,
            error: Some("backend died".to_string()),
            unavailable: Vec::new(),
        };
        let report = summarize(
            &BenchmarkRequest {
                repeats: 4,
                ..request()
            },
            None,
            1,
            vec![
                usable(50, 1_000, 101),
                failed.clone(),
                failed,
                usable(120, 1_000, 101),
            ],
            PeakMemory::measure(None, None, None),
        );

        let ttft = report
            .time_to_first_token_ms
            .expect("one counted repeat survived");
        assert_eq!(
            (ttft.n, ttft.min),
            (1, 120.0),
            "only the one usable non-warm-up repeat"
        );
        assert_eq!(ttft.stddev, None, "one observation supports no spread");
        let rate_notes: Vec<&TraceFieldNote> = report
            .unavailable
            .iter()
            .filter(|note| note.field == FIELD_DECODE_TOKENS_PER_SECOND)
            .collect();
        assert_eq!(
            rate_notes.len(),
            1,
            "two repeats failing identically is one fact, not two notes: {:?}",
            report.unavailable
        );
    }

    /// A run whose every repeat was warm-up has nothing to report, and says so
    /// rather than returning empty spreads that would render as blanks.
    #[test]
    fn a_run_that_was_all_warmup_says_nothing_was_left_to_measure() {
        // `repeats: 1` is refused by `validated`; reaching `summarize` with it
        // means a caller bypassed the boundary, so the fold must still be honest.
        let report = summarize(
            &BenchmarkRequest {
                repeats: 1,
                ..request()
            },
            None,
            1,
            vec![usable(80, 500, 51)],
            PeakMemory::measure(None, None, None),
        );
        assert!(report.time_to_first_token_ms.is_none());
        assert!(report.decode_tokens_per_second.is_none());
        assert!(
            report
                .unavailable
                .iter()
                .any(|note| note.reason.contains("nothing was left to measure")),
            "got {:?}",
            report.unavailable
        );
    }

    /// One repeat has no spread. Reporting `0.0` would read as "perfectly
    /// repeatable", which is the opposite of what one sample supports.
    #[test]
    fn a_single_repeat_has_no_standard_deviation() {
        let single = spread(&[42.0]).expect("one value still has a min, median and max");
        assert_eq!(
            (single.n, single.min, single.median, single.max),
            (1, 42.0, 42.0, 42.0)
        );
        assert_eq!(single.stddev, None);
        assert!(spread(&[]).is_none(), "there is no spread of nothing");
    }

    /// Median over an even count is the mean of the middle pair, and the
    /// standard deviation is the sample one — dividing by `n` instead of `n - 1`
    /// would understate the spread of repeats.
    #[test]
    fn spread_reports_sample_standard_deviation_and_an_interpolated_median() {
        let s = spread(&[10.0, 2.0, 8.0, 4.0]).expect("four values");
        assert_eq!((s.n, s.min, s.max), (4, 2.0, 10.0));
        assert_eq!(s.median, 6.0, "mean of the middle pair 4 and 8");
        // mean 6; deviations -4,-2,2,4; sum of squares 40; /3 = 13.333…
        let stddev = s.stddev.expect("four values have a spread");
        assert!(
            (stddev - (40.0f64 / 3.0).sqrt()).abs() < 1e-9,
            "expected the n-1 divisor, got {stddev}"
        );
    }
}
