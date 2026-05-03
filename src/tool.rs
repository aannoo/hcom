use std::str::FromStr;

const CLAUDE_HOOKS: &[&str] = &[
    "poll",
    "notify",
    "permission-request",
    "pre",
    "post",
    "sessionstart",
    "userpromptsubmit",
    "sessionend",
    "subagent-start",
    "subagent-stop",
];

const GEMINI_HOOKS: &[&str] = &[
    "gemini-sessionstart",
    "gemini-beforeagent",
    "gemini-afteragent",
    "gemini-beforetool",
    "gemini-aftertool",
    "gemini-notification",
    "gemini-sessionend",
];

const CODEX_HOOKS: &[&str] = &[
    "codex-sessionstart",
    "codex-userpromptsubmit",
    "codex-pretooluse",
    "codex-posttooluse",
    "codex-stop",
];

const OPENCODE_HOOKS: &[&str] = &[
    "opencode-start",
    "opencode-status",
    "opencode-read",
    "opencode-stop",
];

const KILO_HOOKS: &[&str] = &[
    "kilocode-start",
    "kilocode-status",
    "kilocode-read",
    "kilocode-stop",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Claude,
    Gemini,
    Codex,
    OpenCode,
    Kilo,
    Adhoc,
}

impl Tool {
    pub fn ready_pattern(&self) -> &'static [u8] {
        match self {
            Tool::Claude => b"? for shortcuts",
            Tool::Codex => "\u{203A} ".as_bytes(),
            Tool::Gemini => b"Type your message",
            Tool::OpenCode => b"ctrl+p commands",
            Tool::Kilo => b"ctrl+p commands",
            Tool::Adhoc => b"",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Gemini => "gemini",
            Tool::Codex => "codex",
            Tool::OpenCode => "opencode",
            Tool::Kilo => "kilo",
            Tool::Adhoc => "adhoc",
        }
    }

    pub fn hooks(&self) -> &'static [&'static str] {
        match self {
            Tool::Claude => CLAUDE_HOOKS,
            Tool::Gemini => GEMINI_HOOKS,
            Tool::Codex => CODEX_HOOKS,
            Tool::OpenCode => OPENCODE_HOOKS,
            Tool::Kilo => KILO_HOOKS,
            Tool::Adhoc => &[],
        }
    }

    pub fn owns_hook(&self, name: &str) -> bool {
        self.hooks().contains(&name)
    }

    pub fn from_hook_name(name: &str) -> Option<Self> {
        [Tool::Claude, Tool::Gemini, Tool::Codex, Tool::OpenCode, Tool::Kilo]
            .into_iter()
            .find(|tool| tool.owns_hook(name))
    }

    pub fn is_hook_name(name: &str) -> bool {
        Self::from_hook_name(name).is_some()
    }
}

impl FromStr for Tool {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "claude" => Ok(Tool::Claude),
            "gemini" => Ok(Tool::Gemini),
            "codex" => Ok(Tool::Codex),
            "opencode" => Ok(Tool::OpenCode),
            "kilo" | "kilocode" => Ok(Tool::Kilo),
            "adhoc" => Ok(Tool::Adhoc),
            _ => Err(format!("Unknown tool: {}", s)),
        }
    }
}

impl std::fmt::Display for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn adhoc_has_no_hooks() {
        assert!(Tool::Adhoc.hooks().is_empty());
        assert_ne!(Tool::from_hook_name("poll"), Some(Tool::Adhoc));
    }

    #[test]
    fn hook_names_are_disjoint() {
        let mut owners = HashMap::new();
        for tool in [Tool::Claude, Tool::Gemini, Tool::Codex, Tool::OpenCode, Tool::Kilo] {
            for hook in tool.hooks() {
                assert_eq!(
                    owners.insert(*hook, tool),
                    None,
                    "{hook} has multiple owners"
                );
                assert_eq!(Tool::from_hook_name(hook), Some(tool));
            }
        }
    }

    #[test]
    fn kilocode_parses_to_kilo() {
        assert_eq!("kilocode".parse::<Tool>().unwrap(), Tool::Kilo);
        assert_eq!("kilo".parse::<Tool>().unwrap(), Tool::Kilo);
    }
}
