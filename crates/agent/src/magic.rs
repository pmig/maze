//! Maze Magic: one prompt to an agent that improves a draft, in an ephemeral session
//! ([`crate::SessionConfig::ephemeral`]). The agent gets the draft and the conversation
//! in the prompt itself. It has no session token, so it can't reach any other mail,
//! every tool permission it asks for is denied and nothing of the run is stored.

use std::time::Duration;

use mazemail_types::Sensitive;
use settings::Settings;

use crate::manager::{AgentManager, StartError, StartRequest};
use crate::session::{SessionEvent, SessionStatus};
use crate::transcript::{EntryUpdate, NoticeLevel};

/// How long a run may take before it is given up.
pub const MAGIC_TIMEOUT: Duration = Duration::from_secs(180);

const OPEN: &str = "<maze-draft>";
const CLOSE: &str = "</maze-draft>";

/// What to improve, and with what.
#[derive(Debug, Clone)]
pub struct MagicRequest {
    /// The agent (id or alias).
    pub agent_id: String,
    /// The user's instructions (`maze_magic.prompt`).
    pub instructions: String,
    /// The user's own text: the part of the body above the quote.
    pub draft: Sensitive<String>,
    /// The quote or forwarded message below it, which stays as it is.
    pub quoted: Sensitive<String>,
    /// The conversation the draft replies to, as Markdown.
    pub thread: Option<Sensitive<String>>,
    /// Current settings.
    pub settings: Settings,
}

/// Why a run gave no draft.
#[derive(Debug, thiserror::Error)]
pub enum MagicError {
    /// The agent couldn't be started.
    #[error(transparent)]
    Start(#[from] StartError),
    /// The agent wants a sign-in first.
    #[error("{0} needs you to sign in first: open the agent column and sign in there")]
    SignInRequired(String),
    /// The agent reported an error.
    #[error("{0}")]
    Agent(String),
    /// The agent stopped without an error.
    #[error("the agent stopped before it answered")]
    Ended,
    /// The answer was empty.
    #[error("the agent's answer was empty")]
    Empty,
    /// It took longer than [`MAGIC_TIMEOUT`].
    #[error("the agent took too long to answer")]
    TimedOut,
}

/// The prompt the agent gets.
pub fn build_prompt(
    instructions: &str,
    draft: &Sensitive<String>,
    quoted: &Sensitive<String>,
    thread: Option<&Sensitive<String>>,
) -> Sensitive<String> {
    let mut prompt = String::new();
    prompt.push_str(instructions.trim());
    prompt.push_str(
        "\n\nYou are improving an email draft in the mail app Maze. Answer with the \
         improved draft only, between <maze-draft> and </maze-draft>, without anything \
         before or after it. Don't use tools and don't add a subject line. Keep the \
         draft's formatting (Markdown) and its language.\n\n<draft>\n",
    );
    prompt.push_str(draft.expose().trim_end());
    prompt.push_str("\n</draft>\n");
    let quoted = quoted.expose().trim();
    if !quoted.is_empty() {
        prompt.push_str(
            "\nThe draft ends with this quote, which Maze keeps as it is. Don't repeat \
             it in your answer.\n\n<quoted>\n",
        );
        prompt.push_str(quoted);
        prompt.push_str("\n</quoted>\n");
    }
    if let Some(thread) = thread.map(|thread| thread.expose().trim())
        && !thread.is_empty()
    {
        prompt.push_str("\nThe conversation the draft replies to:\n\n<conversation>\n");
        prompt.push_str(thread);
        prompt.push_str("\n</conversation>\n");
    }
    Sensitive::new(prompt)
}

/// The improved draft from the agent's answer: the text between the markers, or the
/// whole answer when the agent left them out.
pub fn parse_answer(answer: &str) -> Option<Sensitive<String>> {
    let inner = match (answer.find(OPEN), answer.rfind(CLOSE)) {
        (Some(start), Some(end)) if start + OPEN.len() <= end => &answer[start + OPEN.len()..end],
        (Some(start), None) => &answer[start + OPEN.len()..],
        _ => answer,
    };
    let text = inner.trim_matches(|c| c == '\n' || c == '\r').trim_end();
    (!text.trim().is_empty()).then(|| Sensitive::new(text.to_owned()))
}

impl AgentManager {
    /// Runs Maze Magic: starts an ephemeral session, sends the prompt and returns the
    /// improved draft. Dropping the future ends the session.
    pub async fn run_magic(&self, request: MagicRequest) -> Result<Sensitive<String>, MagicError> {
        match tokio::time::timeout(MAGIC_TIMEOUT, self.run_magic_inner(request)).await {
            Ok(result) => result,
            Err(_) => Err(MagicError::TimedOut),
        }
    }

    async fn run_magic_inner(
        &self,
        request: MagicRequest,
    ) -> Result<Sensitive<String>, MagicError> {
        let prompt = build_prompt(
            &request.instructions,
            &request.draft,
            &request.quoted,
            request.thread.as_ref(),
        );
        let agent_name = self
            .entry(&request.settings, &request.agent_id)
            .map_or_else(|| request.agent_id.clone(), |entry| entry.name);
        let started = self
            .start_ephemeral(StartRequest {
                agent_id: request.agent_id,
                mail_thread: None,
                resume: None,
                settings: request.settings,
            })
            .await?;
        let handle = started.handle;
        let events = handle.events();
        let mut prompt = Some(prompt);
        let mut working = false;
        let mut answer = String::new();
        let mut error: Option<String> = None;
        while let Ok(event) = events.recv().await {
            match event {
                SessionEvent::Status(SessionStatus::Ready) => {
                    if let Some(prompt) = prompt.take() {
                        handle.prompt(prompt.into_inner());
                    } else if working {
                        break;
                    }
                }
                SessionEvent::Status(SessionStatus::Working) => working = true,
                SessionEvent::SignInRequired(_) => {
                    return Err(MagicError::SignInRequired(agent_name));
                }
                SessionEvent::Update(EntryUpdate::AgentChunk(text)) => answer.push_str(&text),
                SessionEvent::Update(EntryUpdate::Notice(text, NoticeLevel::Error)) => {
                    error = Some(text);
                }
                SessionEvent::Ended(Some(message)) => return Err(MagicError::Agent(message)),
                SessionEvent::Ended(None) => return Err(MagicError::Ended),
                _ => {}
            }
        }
        handle.shutdown();
        match parse_answer(&answer) {
            Some(draft) => Ok(draft),
            None => Err(error.map_or(MagicError::Empty, MagicError::Agent)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> Sensitive<String> {
        Sensitive::new(text.to_owned())
    }

    #[test]
    fn the_prompt_holds_instructions_draft_quote_and_conversation() {
        let prompt = build_prompt(
            "Fix typos.",
            &s("Hi Tom,\n\nthre months is fine.\n"),
            &s("On Tue, Tom wrote:\n> Three or six months?"),
            Some(&s("# Contract draft\n\nThree or six months?")),
        );
        insta::assert_snapshot!(prompt.expose());
    }

    #[test]
    fn a_new_message_has_no_quote_or_conversation() {
        let prompt = build_prompt(" Be brief. ", &s("Hello"), &s(""), None);
        let prompt = prompt.expose();
        assert!(prompt.starts_with("Be brief.\n\n"));
        assert!(prompt.contains("<draft>\nHello\n</draft>"));
        assert!(!prompt.contains("<quoted>"));
        assert!(!prompt.contains("<conversation>"));
    }

    #[test]
    fn answers_are_read_between_the_markers() {
        let parse = |answer: &str| parse_answer(answer).map(Sensitive::into_inner);
        assert_eq!(
            parse("Here you go:\n<maze-draft>\nHi Tom,\n\nThree months.\n</maze-draft>\nDone."),
            Some("Hi Tom,\n\nThree months.".to_owned())
        );
        assert_eq!(parse("  Hi Tom  \n"), Some("  Hi Tom".to_owned()));
        assert_eq!(
            parse("<maze-draft>\nHi Tom"),
            Some("Hi Tom".to_owned()),
            "a missing end marker"
        );
        assert_eq!(parse("<maze-draft>\n\n</maze-draft>"), None);
        assert_eq!(parse(""), None);
    }

    mod run {
        use std::sync::Arc;

        use local_api::TokenStore as _;

        use super::*;
        use crate::fake::{FakeAgent, FakeStep};
        use crate::host::{AgentHost as _, MemoryHost};
        use crate::http::tests::MemoryHttp;
        use crate::manager::ManagerConfig;
        use crate::paths::AgentPaths;

        struct Setup {
            _dir: tempfile::TempDir,
            host: Arc<MemoryHost>,
            manager: AgentManager,
            settings: Settings,
        }

        fn setup(agent: FakeAgent) -> Setup {
            let dir = tempfile::tempdir().unwrap();
            let host = Arc::new(MemoryHost::default());
            let manager = AgentManager::new(ManagerConfig {
                paths: AgentPaths::new(dir.path()),
                home: None,
                cli_program: None,
                socket: None,
                host: Arc::clone(&host) as Arc<dyn crate::AgentHost>,
                http: Arc::new(MemoryHttp::default()),
            });
            manager.set_test_agents(Arc::new(move |_| Some(agent.clone())));
            let mut settings = Settings::default();
            settings.agent_sandbox.enabled = false;
            settings.agent.custom.insert(
                "fake".to_owned(),
                settings::CustomAgentSettings {
                    name: Some("Fake".to_owned()),
                    command: "/bin/sh".to_owned(),
                    ..Default::default()
                },
            );
            Setup {
                _dir: dir,
                host,
                manager,
                settings,
            }
        }

        fn request(settings: &Settings) -> MagicRequest {
            MagicRequest {
                agent_id: "fake".to_owned(),
                instructions: "Fix typos.".to_owned(),
                draft: s("Hi Tom,\n\nthre months."),
                quoted: s("> Three or six?"),
                thread: None,
                settings: settings.clone(),
            }
        }

        #[tokio::test]
        async fn the_improved_draft_comes_back_and_nothing_is_kept() {
            let agent = FakeAgent::new(vec![
                FakeStep::Thought("Fixing.".into()),
                FakeStep::AskPermission {
                    title: "Read a file".into(),
                },
                FakeStep::Message("<maze-draft>\nHi Tom,\n\n".into()),
                FakeStep::Message("Three months.\n</maze-draft>".into()),
            ]);
            let setup = setup(agent.clone());
            let draft = setup
                .manager
                .run_magic(request(&setup.settings))
                .await
                .unwrap();
            assert_eq!(draft.expose(), "Hi Tom,\n\nThree months.");
            let log = agent.log();
            assert!(
                log.iter().any(|l| l == "permission:Read a file:reject"),
                "{log:?}"
            );
            let prompt = log.iter().find(|l| l.starts_with("prompt:")).unwrap();
            assert!(prompt.contains("thre months."));
            assert!(prompt.contains("> Three or six?"));
            // No stored conversation and no session token.
            assert!(
                setup
                    .host
                    .agent_conversations(None)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(setup.host.tokens().list().await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn an_empty_answer_is_an_error() {
            let setup = setup(FakeAgent::new(vec![FakeStep::Message("  ".into())]));
            let error = setup
                .manager
                .run_magic(request(&setup.settings))
                .await
                .unwrap_err();
            assert!(matches!(error, MagicError::Empty), "{error:?}");
        }

        #[tokio::test]
        async fn an_unknown_agent_is_an_error() {
            let setup = setup(FakeAgent::new(Vec::new()));
            let mut request = request(&setup.settings);
            request.agent_id = "nobody".to_owned();
            let error = setup.manager.run_magic(request).await.unwrap_err();
            assert!(
                matches!(error, MagicError::Start(StartError::UnknownAgent(_))),
                "{error:?}"
            );
        }
    }
}
