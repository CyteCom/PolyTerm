//! Log subscriber setup, including the redaction filter NFR-8 requires.
//!
//! There are two defences against a secret reaching a log, and this is the
//! second one. The first is [`polyterm_core::Secret`], which cannot be printed
//! at all. This layer catches what the first misses: a bare `String` logged
//! under a field name that says what it is.
//!
//! It is here rather than in `polyterm-core` so that `tracing-subscriber` stays
//! out of the dependency tree of every backend crate. The *policy* — which
//! field names are secret — lives in the core crate, so both halves agree.

use std::fmt;

use polyterm_core::is_secret_field;
use tracing::field::{Field, Visit};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::FormatFields;
use tracing_subscriber::fmt::format::Writer;

/// Field formatter that replaces the value of any secret-named field with
/// `<redacted>`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RedactingFields;

struct RedactingVisitor<'a> {
    writer: Writer<'a>,
    result: fmt::Result,
    first: bool,
}

impl RedactingVisitor<'_> {
    fn separate(&mut self) {
        if self.result.is_err() {
            return;
        }
        if self.first {
            self.first = false;
        } else {
            self.result = write!(self.writer, " ");
        }
    }
}

impl Visit for RedactingVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        // Routed through record_debug so string values are quoted the way the
        // default formatter quotes them.
        self.record_debug(field, &value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.separate();
        if self.result.is_err() {
            return;
        }
        let name = field.name();
        self.result = if is_secret_field(name) {
            write!(self.writer, "{name}=<redacted>")
        } else if name == "message" {
            write!(self.writer, "{value:?}")
        } else {
            write!(self.writer, "{name}={value:?}")
        };
    }
}

impl<'writer> FormatFields<'writer> for RedactingFields {
    fn format_fields<R: RecordFields>(&self, writer: Writer<'writer>, fields: R) -> fmt::Result {
        let mut visitor = RedactingVisitor {
            writer,
            result: Ok(()),
            first: true,
        };
        fields.record(&mut visitor);
        visitor.result
    }
}

/// Install the global subscriber. Level is controlled by `POLYTERM_LOG`, which
/// takes the usual `RUST_LOG` syntax.
pub fn init() {
    let filter = EnvFilter::try_from_env("POLYTERM_LOG").unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .fmt_fields(RedactingFields)
        .with_env_filter(filter)
        .init();
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn render(emit: impl FnOnce()) -> String {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .fmt_fields(RedactingFields)
            .with_writer(capture.clone())
            .without_time()
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, emit);

        let bytes = capture.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn password_field_is_redacted() {
        let out = render(|| {
            tracing::info!(host = "example.net", password = "hunter2", "connecting");
        });
        assert!(out.contains("password=<redacted>"), "{out}");
        assert!(!out.contains("hunter2"), "{out}");
    }

    #[test]
    fn passphrase_and_token_are_redacted() {
        let out = render(|| {
            tracing::info!(
                key_passphrase = "s3cret",
                auth_token = "abc123",
                "authenticating"
            );
        });
        assert!(!out.contains("s3cret"), "{out}");
        assert!(!out.contains("abc123"), "{out}");
        assert!(out.contains("key_passphrase=<redacted>"), "{out}");
        assert!(out.contains("auth_token=<redacted>"), "{out}");
    }

    #[test]
    fn ordinary_fields_and_the_message_survive() {
        let out = render(|| {
            tracing::info!(host = "example.net", cols = 80, rows = 24, "resized");
        });
        assert!(out.contains("resized"), "{out}");
        assert!(out.contains("example.net"), "{out}");
        assert!(out.contains("cols=80"), "{out}");
        assert!(out.contains("rows=24"), "{out}");
    }

    #[test]
    fn redaction_survives_a_secret_arriving_as_a_debug_value() {
        // Not every secret field is recorded as a str; a struct or an integer
        // takes the record_debug path instead.
        let out = render(|| {
            tracing::info!(api_key = 12345, "token minted");
        });
        assert!(!out.contains("12345"), "{out}");
        assert!(out.contains("api_key=<redacted>"), "{out}");
    }
}
