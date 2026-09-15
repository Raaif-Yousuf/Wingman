pub mod anthropic;
pub mod openai;

pub use anthropic::Anthropic;
pub use openai::OpenAi;
/// The system prompt. The user solves physics and statistics problems on paper,
/// then screenshots the on-screen assignment to check their result before
/// entering it. The screenshot shows the problem — not, usually, their working.
pub const DEFAULT_PROMPT: &str = "You are shown a screenshot of the user's screen. They are working through a physics or statistics problem and want a second opinion before committing an answer. Usually the problem statement is on screen (often an online assignment) while the user has done the working on paper, so their derivation is generally NOT visible to you. Sometimes a value they are about to submit is already typed into an input field, and sometimes their working is on screen too.

Work the problem out yourself from what is visible, then:
- If a candidate answer is visible (typed into a field, or written in on-screen working), compare it against your own result. Say plainly whether it matches. If it does not, give the correct value and name the specific mistake you can infer (e.g. \"that is cos 30, not sin 30\" or \"you used the population variance formula, not the sample one\").
- If only the problem is visible, just give your answer.
- If something essential is unreadable or missing from the screenshot, say exactly what you need instead of guessing at it.

Carry units through and give the final value to a sensible number of significant figures.

Respond with exactly two fields, and write them in this order:
- detail: FIRST. At most 700 characters, plain text. Work the problem through here step by step so the user can check it against their own. This is your scratchpad — reason it out before committing to a verdict.
- headline: SECOND, and it must be the conclusion of the working you just wrote. At most 90 characters, plain text. Lead with the final value, or with the correction if a visible answer is wrong. Never state a verdict in the headline that your own detail contradicts; if the working changed your mind, the headline follows the working.

Use plain text only in both fields: no markdown (no asterisks, backticks, headers or bullet characters) and no LaTeX. This renders in a plain GDI text window that can display neither. Write powers as m/s^2 and fractions inline.";

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Answer {
    /// <= 90 chars. Leads with the final value or the correction.
    pub headline: String,
    /// <= 700 chars of plain-text working. May be empty.
    pub detail: String,
}

#[derive(Debug)]
pub struct Shot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn ask(&self, shot: &Shot, prompt: &str) -> anyhow::Result<Answer>;

    /// Whether this provider is usable, e.g. has a non-empty API key.
    ///
    /// Defaults to `true`. A provider backed by a key the user has not
    /// configured overrides this to return `false` so that `Chain` can skip
    /// it silently instead of trying it and recording a failure.
    fn ready(&self) -> bool {
        true
    }
}

/// Runs a list of providers in order, falling through to the next on any
/// failure (transport error, non-2xx, or unparseable body). A provider that
/// is not `ready()` (e.g. its API key is empty) is skipped entirely, rather
/// than being tried and failing. If every provider fails or is skipped, the
/// *first* encountered error is surfaced.
pub struct Chain {
    providers: Vec<Box<dyn Provider>>,
}

impl Chain {
    pub fn new(providers: Vec<Box<dyn Provider>>) -> Self {
        Self { providers }
    }

    pub fn ask(&self, shot: &Shot, prompt: &str) -> anyhow::Result<Answer> {
        let mut first_err: Option<anyhow::Error> = None;

        for provider in &self.providers {
            if !provider.ready() {
                continue;
            }
            match provider.ask(shot, prompt) {
                Ok(answer) => return Ok(answer),
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }

        Err(first_err.unwrap_or_else(|| anyhow::anyhow!("no providers configured")))
    }

    /// Names of every provider in the chain, in order, regardless of
    /// readiness. Mainly useful for introspection/testing and for surfacing
    /// the active provider in UI (e.g. a tray tooltip).
    pub fn provider_names(&self) -> Vec<&'static str> {
        self.providers.iter().map(|p| p.name()).collect()
    }

    /// Names of only the providers that are currently `ready()` (e.g. have a
    /// non-empty API key), in order.
    pub fn ready_provider_names(&self) -> Vec<&'static str> {
        self.providers
            .iter()
            .filter(|p| p.ready())
            .map(|p| p.name())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct MockProvider {
        name: &'static str,
        ready: bool,
        calls: AtomicU32,
        result: fn() -> anyhow::Result<Answer>,
    }

    impl Provider for MockProvider {
        fn name(&self) -> &'static str {
            self.name
        }

        fn ready(&self) -> bool {
            self.ready
        }

        fn ask(&self, _shot: &Shot, _prompt: &str) -> anyhow::Result<Answer> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (self.result)()
        }
    }

    fn ok_answer() -> anyhow::Result<Answer> {
        Ok(Answer {
            headline: "42".into(),
            detail: "because reasons".into(),
        })
    }

    fn shot() -> Shot {
        Shot {
            png: vec![],
            width: 1,
            height: 1,
        }
    }

    #[test]
    fn tries_providers_in_order_and_returns_first_success() {
        let calls_a = AtomicU32::new(0);
        let a = MockProvider {
            name: "a",
            ready: true,
            calls: calls_a,
            result: || Err(anyhow::anyhow!("a failed")),
        };
        let b = MockProvider {
            name: "b",
            ready: true,
            calls: AtomicU32::new(0),
            result: ok_answer,
        };
        let chain = Chain::new(vec![Box::new(a), Box::new(b)]);
        let answer = chain.ask(&shot(), "prompt").unwrap();
        assert_eq!(answer.headline, "42");
    }

    #[test]
    fn skips_provider_with_empty_key_without_failing() {
        struct PanicsIfCalled;
        impl Provider for PanicsIfCalled {
            fn name(&self) -> &'static str {
                "skip-me"
            }
            fn ready(&self) -> bool {
                false
            }
            fn ask(&self, _shot: &Shot, _prompt: &str) -> anyhow::Result<Answer> {
                panic!("unready provider must not be asked");
            }
        }

        let good = MockProvider {
            name: "good",
            ready: true,
            calls: AtomicU32::new(0),
            result: ok_answer,
        };
        let chain = Chain::new(vec![Box::new(PanicsIfCalled), Box::new(good)]);
        let answer = chain.ask(&shot(), "prompt").unwrap();
        assert_eq!(answer.headline, "42");
    }

    #[test]
    fn all_unready_surfaces_no_providers_configured() {
        struct NeverReady;
        impl Provider for NeverReady {
            fn name(&self) -> &'static str {
                "never"
            }
            fn ready(&self) -> bool {
                false
            }
            fn ask(&self, _shot: &Shot, _prompt: &str) -> anyhow::Result<Answer> {
                unreachable!("should never be called when not ready")
            }
        }

        let chain = Chain::new(vec![Box::new(NeverReady), Box::new(NeverReady)]);
        let err = chain.ask(&shot(), "prompt").unwrap_err();
        assert_eq!(err.to_string(), "no providers configured");
    }

    #[test]
    fn surfaces_first_error_when_all_fail() {
        let a = MockProvider {
            name: "a",
            ready: true,
            calls: AtomicU32::new(0),
            result: || Err(anyhow::anyhow!("first error")),
        };
        let b = MockProvider {
            name: "b",
            ready: true,
            calls: AtomicU32::new(0),
            result: || Err(anyhow::anyhow!("second error")),
        };
        let chain = Chain::new(vec![Box::new(a), Box::new(b)]);
        let err = chain.ask(&shot(), "prompt").unwrap_err();
        assert_eq!(err.to_string(), "first error");
    }

    #[test]
    fn skip_does_not_become_first_error() {
        struct NeverReady;
        impl Provider for NeverReady {
            fn name(&self) -> &'static str {
                "never"
            }
            fn ready(&self) -> bool {
                false
            }
            fn ask(&self, _shot: &Shot, _prompt: &str) -> anyhow::Result<Answer> {
                unreachable!("should never be called when not ready")
            }
        }
        let a = MockProvider {
            name: "a",
            ready: true,
            calls: AtomicU32::new(0),
            result: || Err(anyhow::anyhow!("real error")),
        };
        let chain = Chain::new(vec![Box::new(NeverReady), Box::new(a)]);
        let err = chain.ask(&shot(), "prompt").unwrap_err();
        assert_eq!(err.to_string(), "real error");
    }
}
