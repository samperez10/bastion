//! Agent integration boundary.
//!
//! The pane manager deliberately knows nothing about a particular AI CLI. An
//! adapter owns that CLI's launch syntax, resume syntax, and optional visual
//! identification. Visual identification is display-only: it never writes a
//! session record and therefore can never cause an automatic resume.

pub trait AgentAdapter: Sync {
    fn id(&self) -> &'static str;
    fn executable(&self) -> &'static str;
    fn resume_arguments(&self, session_id: &str) -> Vec<String>;
    fn screen_matches(&self, screen: &str) -> bool;

    fn resume_command(&self, session_id: &str) -> String {
        let arguments = self.resume_arguments(session_id);
        std::iter::once(self.executable())
            .chain(arguments.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

struct Claude;

impl AgentAdapter for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }
    fn executable(&self) -> &'static str {
        "claude"
    }
    fn resume_arguments(&self, session_id: &str) -> Vec<String> {
        vec!["--resume".to_owned(), session_id.to_owned()]
    }
    fn screen_matches(&self, screen: &str) -> bool {
        screen.contains("Claude Code v")
    }
}

struct Codex;

impl AgentAdapter for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn executable(&self) -> &'static str {
        "codex"
    }
    fn resume_arguments(&self, session_id: &str) -> Vec<String> {
        vec!["resume".to_owned(), session_id.to_owned()]
    }
    fn screen_matches(&self, screen: &str) -> bool {
        screen.contains("OpenAI Codex")
    }
}

/// Google Antigravity CLI. Its conversation IDs can be supplied by the user
/// or a future official integration; unlike Codex, we do not scrape private
/// files to guess them.
struct Antigravity;

impl AgentAdapter for Antigravity {
    fn id(&self) -> &'static str {
        "antigravity"
    }
    fn executable(&self) -> &'static str {
        "agy"
    }
    fn resume_arguments(&self, session_id: &str) -> Vec<String> {
        vec![format!("--conversation={session_id}")]
    }
    fn screen_matches(&self, screen: &str) -> bool {
        // Verified against the native Termux TUI in terminal-lab. This is
        // display-only; persistence still requires an actual conversation ID.
        screen.contains("Antigravity CLI")
    }
}

static CLAUDE: Claude = Claude;
static CODEX: Codex = Codex;
static ANTIGRAVITY: Antigravity = Antigravity;
static ADAPTERS: [&'static dyn AgentAdapter; 3] = [&CLAUDE, &CODEX, &ANTIGRAVITY];

pub fn named(id: &str) -> Option<&'static dyn AgentAdapter> {
    ADAPTERS.iter().copied().find(|adapter| adapter.id() == id)
}

pub fn for_executable(command: &str) -> Option<&'static dyn AgentAdapter> {
    ADAPTERS
        .iter()
        .copied()
        .find(|adapter| adapter.executable() == command)
}

pub fn detect_screen(screen: &str) -> Option<&'static str> {
    ADAPTERS
        .iter()
        .copied()
        .find(|adapter| adapter.screen_matches(screen))
        .map(AgentAdapter::id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapters_own_their_identification() {
        assert_eq!(detect_screen("Claude Code v2.1"), Some("claude"));
        assert_eq!(detect_screen("OpenAI Codex (v0.155)"), Some("codex"));
        assert_eq!(detect_screen("Antigravity CLI 1.2.9"), Some("antigravity"));
        assert_eq!(detect_screen("plain interactive shell"), None);
    }

    #[test]
    fn antigravity_uses_its_native_conversation_flag() {
        let adapter = named("antigravity").expect("registered adapter");
        assert_eq!(adapter.executable(), "agy");
        assert_eq!(
            adapter.resume_arguments("6622b838-513d-436b-93ae-7af3f85a1977"),
            ["--conversation=6622b838-513d-436b-93ae-7af3f85a1977"]
        );
    }
}
