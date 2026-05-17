//! Structured handoff prompt body.
//!
//! A handoff effect surfaces a *prompt* to its caller — the most
//! externally-visible artifact a binary in this family produces.
//! [`HandoffPrompt`] gives that artifact structural shape so it is
//! addressable component-by-component rather than as a free string:
//!
//! * `headline` — the first-read one-line summary. The
//!   [`SingleLineString`] type forbids embedded newlines.
//! * `sections` — ordered components, each variant of
//!   [`PromptSection`] capturing a distinct rendering shape (prose,
//!   numbered list, per-item witnesses with bodies, key/value
//!   triage context).
//!
//! `Display` is the canonical text projection; programmatic
//! consumers can also walk `sections` directly.

use crate::non_empty::NonEmpty;
use crate::single_line_string::SingleLineString;
use serde::Serialize;
use std::fmt;

/// Structured handoff prompt — the payload carried by handoff
/// variants of [`crate::ActionEffect`] and [`crate::HandoffAction`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HandoffPrompt {
    pub headline: SingleLineString,
    pub sections: Vec<PromptSection>,
}

/// One structured component of a [`HandoffPrompt`]. Each variant
/// captures a recurring rendering shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PromptSection {
    /// Free prose paragraph. May contain embedded newlines.
    Paragraph(String),
    /// Numbered list — `1. <item>` / `2. <item>` / … Items are
    /// individually single-line. For multi-line entries with
    /// bodies, prefer [`Self::Witnesses`].
    NumberedList(NonEmpty<SingleLineString>),
    /// Per-item witnesses — each carries a one-line label and a
    /// free-form body. Used when the recipient needs both a stable
    /// identifier and full content per item.
    Witnesses(NonEmpty<Witness>),
    /// Key/value triage context — `<key>: <value>` lines, both
    /// sides single-line so the block stays regex-friendly.
    Context(NonEmpty<ContextLine>),
}

/// One witness in a [`PromptSection::Witnesses`] section.
///
/// `url: Some(_)` renders as a trailing `URL: <url>` line; `None`
/// omits the line entirely. A bare `URL:` header is unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Witness {
    pub label: SingleLineString,
    pub body: String,
    pub url: Option<String>,
}

/// One `<key>: <value>` line in a [`PromptSection::Context`]
/// block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextLine {
    pub key: SingleLineString,
    pub value: SingleLineString,
}

impl HandoffPrompt {
    /// Construct a prompt with only a headline. Sections can be
    /// added via `push_*` helpers or by extending the
    /// `sections` field directly.
    pub fn new(headline: impl Into<SingleLineString>) -> Self {
        Self {
            headline: headline.into(),
            sections: Vec::new(),
        }
    }

    /// Append a free-form prose paragraph.
    pub fn push_paragraph(&mut self, text: impl Into<String>) {
        self.sections.push(PromptSection::Paragraph(text.into()));
    }

    /// Append a numbered list. Caller is responsible for ensuring
    /// each entry is single-line; the type enforces non-empty.
    pub fn push_numbered_list(&mut self, items: NonEmpty<SingleLineString>) {
        self.sections.push(PromptSection::NumberedList(items));
    }

    /// Append a witness block.
    pub fn push_witnesses(&mut self, items: NonEmpty<Witness>) {
        self.sections.push(PromptSection::Witnesses(items));
    }

    /// Append a context line. Coalesces with a trailing
    /// `Context` section if present, otherwise starts a new one.
    /// The coalescing rule lets multiple boundary decorators each
    /// add their own lines without producing fragmented blocks.
    pub fn push_context_line(
        &mut self,
        key: impl Into<SingleLineString>,
        value: impl Into<SingleLineString>,
    ) {
        let line = ContextLine {
            key: key.into(),
            value: value.into(),
        };
        match self.sections.last_mut() {
            Some(PromptSection::Context(lines)) => lines.push(line),
            _ => self
                .sections
                .push(PromptSection::Context(NonEmpty::singleton(line))),
        }
    }

    /// Chainable form of [`Self::push_paragraph`] for
    /// expression-position construction.
    #[must_use]
    pub fn with_paragraph(mut self, text: impl Into<String>) -> Self {
        self.push_paragraph(text);
        self
    }

    /// Chainable form of [`Self::push_numbered_list`].
    #[must_use]
    pub fn with_numbered_list(mut self, items: NonEmpty<SingleLineString>) -> Self {
        self.push_numbered_list(items);
        self
    }

    /// Chainable form of [`Self::push_witnesses`].
    #[must_use]
    pub fn with_witnesses(mut self, items: NonEmpty<Witness>) -> Self {
        self.push_witnesses(items);
        self
    }

    /// Chainable form of [`Self::push_context_line`].
    #[must_use]
    pub fn with_context_line(
        mut self,
        key: impl Into<SingleLineString>,
        value: impl Into<SingleLineString>,
    ) -> Self {
        self.push_context_line(key, value);
        self
    }
}

impl fmt::Display for HandoffPrompt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.headline.as_str())?;
        for section in &self.sections {
            f.write_str("\n\n")?;
            fmt::Display::fmt(section, f)?;
        }
        Ok(())
    }
}

impl fmt::Display for PromptSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Paragraph(s) => f.write_str(s),
            Self::NumberedList(items) => {
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str("\n")?;
                    }
                    write!(f, "{}. {}", i + 1, item)?;
                }
                Ok(())
            }
            Self::Witnesses(items) => {
                for (i, w) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str("\n\n")?;
                    }
                    write!(f, "{}\n{}", w.label, w.body)?;
                    if let Some(url) = &w.url {
                        write!(f, "\nURL: {url}")?;
                    }
                }
                Ok(())
            }
            Self::Context(lines) => {
                for (i, l) in lines.iter().enumerate() {
                    if i > 0 {
                        f.write_str("\n")?;
                    }
                    write!(f, "{}: {}", l.key, l.value)?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headline_only_renders_one_line() {
        let p = HandoffPrompt::new("Request or self-approve");
        assert_eq!(format!("{p}"), "Request or self-approve");
    }

    #[test]
    fn headline_plus_paragraph_separates_with_blank_line() {
        let mut p = HandoffPrompt::new("Address summary-only change-request review.");
        p.push_paragraph("Read the latest CHANGES_REQUESTED review body and address it.");
        let s = format!("{p}");
        assert_eq!(
            s,
            "Address summary-only change-request review.\n\
             \n\
             Read the latest CHANGES_REQUESTED review body and address it."
        );
    }

    #[test]
    fn context_lines_join_with_single_newline_within_block() {
        let mut p = HandoffPrompt::new("Halted for human triage.");
        p.push_context_line("PR", "https://github.com/acme/widget/pull/42");
        p.push_context_line("Blocker", "ci_fail: Lint");
        let s = format!("{p}");
        assert_eq!(
            s,
            "Halted for human triage.\n\
             \n\
             PR: https://github.com/acme/widget/pull/42\n\
             Blocker: ci_fail: Lint"
        );
    }

    #[test]
    fn push_context_line_extends_trailing_context_section() {
        let mut p = HandoffPrompt::new("h");
        p.push_context_line("PR", "url1");
        p.push_context_line("Blocker", "b1");
        // Both lines land in the same Context section, not two
        // separate ones (otherwise the renderer would emit a blank
        // line between them).
        assert_eq!(p.sections.len(), 1);
        match &p.sections[0] {
            PromptSection::Context(lines) => assert_eq!(lines.len(), 2),
            other => panic!("expected Context, got {other:?}"),
        }
    }

    #[test]
    fn push_context_line_starts_new_section_after_non_context() {
        let mut p = HandoffPrompt::new("h");
        p.push_paragraph("para");
        p.push_context_line("PR", "url");
        // Paragraph then Context — two sections.
        assert_eq!(p.sections.len(), 2);
    }

    #[test]
    fn witnesses_render_with_label_then_body_double_separated() {
        let witnesses = NonEmpty::try_from_vec(vec![
            Witness {
                label: "Copilot @ src/foo.rs:42    thread_id: t1".into(),
                body: "Consider a different name here.".into(),
                url: None,
            },
            Witness {
                label: "Cursor @ src/bar.rs:7    thread_id: t2".into(),
                body: "Multi-line\nbody.".into(),
                url: None,
            },
        ])
        .unwrap();
        let mut p = HandoffPrompt::new("Address 2 unresolved review threads.");
        p.push_witnesses(witnesses);
        let s = format!("{p}");
        assert!(
            s.contains("Copilot @ src/foo.rs:42    thread_id: t1\nConsider a different name here.")
        );
        assert!(s.contains("Cursor @ src/bar.rs:7    thread_id: t2\nMulti-line\nbody."));
    }

    #[test]
    fn witness_with_url_none_renders_without_url_line() {
        let witnesses = NonEmpty::singleton(Witness {
            label: "label".into(),
            body: "body line".into(),
            url: None,
        });
        let mut p = HandoffPrompt::new("h");
        p.push_witnesses(witnesses);
        let s = format!("{p}");
        assert_eq!(s, "h\n\nlabel\nbody line");
        assert!(!s.contains("URL:"));
    }

    #[test]
    fn witness_with_url_some_renders_url_line_below_body() {
        let witnesses = NonEmpty::singleton(Witness {
            label: "label".into(),
            body: "body line".into(),
            url: Some("https://example/r/1".into()),
        });
        let mut p = HandoffPrompt::new("h");
        p.push_witnesses(witnesses);
        let s = format!("{p}");
        assert_eq!(s, "h\n\nlabel\nbody line\nURL: https://example/r/1");
    }

    #[test]
    fn numbered_list_renders_with_one_based_indices() {
        let items = NonEmpty::try_from_vec(vec![
            SingleLineString::new("first"),
            SingleLineString::new("second"),
            SingleLineString::new("third"),
        ])
        .unwrap();
        let mut p = HandoffPrompt::new("Do these in order.");
        p.push_numbered_list(items);
        let s = format!("{p}");
        assert!(s.contains("1. first\n2. second\n3. third"));
    }

    #[test]
    fn serializes_as_struct_with_sections_array() {
        let mut p = HandoffPrompt::new("h");
        p.push_paragraph("body");
        p.push_context_line("k", "v");
        let json = serde_json::to_string(&p).unwrap();
        // Schema sanity — headline is a string, sections is an
        // array of tagged variants. Recorder + JSONL consumers
        // can rely on this shape.
        assert!(json.contains("\"headline\":\"h\""));
        assert!(json.contains("\"sections\":"));
        assert!(json.contains("\"Paragraph\":\"body\""));
        assert!(json.contains("\"Context\":"));
    }
}
